//! quiverd: the launcher's backend. Owns the Ubisoft session, the Denuvo broker, installed
//! games, per-game settings (with history), install/launch jobs and an activity log.
//! Front ends (`quiver`, the GUI, scripts and agents) talk to it over a Unix socket; see
//! `quiver-api` for the protocol.

mod achievements;
mod downloads;
mod exports;
mod mods;
mod perf;
mod info;
mod lab;
mod lifecycle;
mod localsessions;
mod options;
mod tweaks;
mod savestores;
mod setup;
mod steamgames;
mod steamhook;
mod spaces;
mod steamlib;
mod steammods;
mod stores;
mod epic;
mod epicgame;
mod sightinfo;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use quiver_api::{
    Account, ActivityEntry, Envelope, Event, GameRef, Job, JobKind, LibraryEntry, LoginResult, Reply, Request, SettingOp,
    Status,
};
use quiver_core::auth::{Auth, LoginOutcome, TwoFactorChallenge};
use quiver_core::broker::Broker;
use quiver_core::launcher::{self, Config, Games, LaunchEvent};
use quiver_core::library::Library;
use quiver_core::online::{Accounts, Online};
use quiver_core::playsession::PlaySessions;
use quiver_core::store::{self, AccountRef};
use quiver_core::settings;

const ACTIVITY_KEEP: usize = 1000;

/// Which account a game runs (and installs, syncs saves, reports play time) with: its
/// `account` setting if set; else the default account if it owns the game; else the first
/// account that does. `owners` is in [`order_owners`] order; empty when unknown, which
/// leaves the default. `None`: no account at all.
fn launch_account(setting: Option<&str>, default: Option<&str>, owners: &[String]) -> Option<String> {
    if let Some(id) = setting {
        return Some(id.to_owned());
    }
    match default {
        Some(d) if owners.is_empty() || owners.iter().any(|o| o == d) => Some(d.to_owned()),
        _ => owners.first().cloned().or_else(|| default.map(str::to_owned)),
    }
}

/// Order owners like `Status::accounts`: the default first, then sign-in order.
fn order_owners(owners: &mut [String], accounts: &[AccountRef], default: Option<&str>) {
    let pos = |id: &String| accounts.iter().position(|a| &a.user_id == id).unwrap_or(usize::MAX);
    owners.sort_by_key(|id| (Some(id.as_str()) != default, pos(id)));
}

struct Daemon {
    /// One Ubisoft login (demux socket) per signed-in account.
    accounts: Arc<Accounts>,
    /// For signing in; signed-in sessions refresh through their account's `Online`.
    auth: Arc<Auth>,
    /// The Denuvo/play-session broker, to drop an account's connections on sign-out.
    broker_svc: Arc<Broker>,
    cfg: Arc<Config>,
    broker: String,
    pending_2fa: Mutex<Option<TwoFactorChallenge>>,
    jobs: Mutex<BTreeMap<u64, Job>>,
    next_job: AtomicU64,
    running: Mutex<HashSet<u32>>,
    /// Other stores' games Quiver is running (Epic).
    running_games: Mutex<HashSet<GameRef>>,
    /// The install queue (installs, updates, repairs) and verify jobs.
    downloads: Arc<downloads::Downloads>,
    library: tokio::sync::Mutex<Option<Vec<LibraryEntry>>>,
    /// Accounts whose library the cached one is missing (fetch failed).
    library_failed: Mutex<Vec<String>>,
    activity: Mutex<VecDeque<ActivityEntry>>,
    events: broadcast::Sender<Event>,
    /// Games being played, from any launch path; reports play time to Ubisoft.
    sessions: Arc<PlaySessions>,
    /// Steam games (not Ubisoft copies) played through `quiver steam-run`: local only.
    local: Arc<localsessions::LocalSessions>,
    /// Mod options: running sessions as options see them, the SDK link, options events.
    options: options::Options,
    tweaks: tweaks::Tweaks,
    /// Mods connected through the SDK pipe (`quiver_mod.h`), via bridges.
    mod_sdk: Arc<quiver_core::modsdk::ModRegistry>,
    /// Performance metrics and the HUD (design/SIGHT-HUD.md): per-session registry,
    /// producers, subscriptions, HUD models.
    perf: Arc<perf::PerfHub>,
    /// Achievement unlock status and reports (shared with the broker).
    achievements: Arc<quiver_core::achievements::Engine>,
    /// Providers of stores other than Ubisoft (see `stores`; Ubisoft's wraps the daemon).
    stores: Vec<Arc<dyn quiver_core::stores::StoreProvider>>,
    /// Epic accounts and library (its provider is in `stores`).
    epic: Arc<epic_core::Epic>,
    /// Restarting onto new builds: what it waits on, holds, refusing work meanwhile.
    lifecycle: Arc<lifecycle::Lifecycle>,
    /// Open client connections (their reply channel, and a signal once a
    /// `Reply::Closing` went out on it), to say goodbye before a restart.
    conns: Mutex<HashMap<u64, ConnHandle>>,
    /// When the pending 2FA challenge was issued (a restart waits a while for its code).
    pending_2fa_at: Mutex<Option<std::time::Instant>>,
}

/// A client connection's reply channel, and a signal once a `Reply::Closing` went out on it.
type ConnHandle = (mpsc::UnboundedSender<Reply>, Arc<tokio::sync::Notify>);

/// The Ubisoft product a request names; other stores' games can't do what it asks (yet).
fn ubisoft_only(game: GameRef) -> Result<u32> {
    game.as_ubisoft().with_context(|| format!("{game}: {} games can't do that in Quiver yet", game.store.label()))
}

/// A game Quiver keeps launch settings for: a Ubisoft product, an Epic game (Quiver launches
/// it: its default profile, env, args, Proton), or a Steam game (applied to its Steam launch
/// by `quiver steam-run`).
fn settings_game(game: GameRef) -> Result<GameRef> {
    match game.store {
        quiver_api::StoreId::Ubisoft | quiver_api::StoreId::Epic | quiver_api::StoreId::Steam => Ok(game),
        s => bail!("{game}: {} games have no Quiver launch settings yet", s.label()),
    }
}

/// Run a (possibly non-`Send`) core future to completion on a blocking thread.
async fn run_local<F, Fut, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>>,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || tokio::runtime::Handle::current().block_on(f())).await?
}

impl Daemon {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn log_activity(&self, source: &str, action: String, product_id: Option<u32>) {
        self.log_activity_game(source, action, product_id.map(GameRef::ubisoft))
    }

    /// [`Self::log_activity`] for a game of any store.
    fn log_activity_game(&self, source: &str, action: String, game: Option<GameRef>) {
        let entry = ActivityEntry { at: chrono::Utc::now(), source: source.to_owned(), action, game };
        tracing::info!(source, game = game.map(|g| g.to_string()), "{}", entry.action);
        let path = launcher::data_dir().join("activity.jsonl");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
        }
        let mut a = self.activity.lock().unwrap();
        a.push_back(entry);
        while a.len() > ACTIVITY_KEEP {
            a.pop_front();
        }
    }

    fn update_job(&self, id: u64, f: impl FnOnce(&mut Job)) {
        let job = {
            let mut jobs = self.jobs.lock().unwrap();
            let Some(job) = jobs.get_mut(&id) else { return };
            f(job);
            job.clone()
        };
        self.emit(Event::Job(job));
    }

    fn new_job(&self, kind: JobKind, product_id: u32, name: String) -> u64 {
        self.new_game_job(kind, GameRef::ubisoft(product_id), name)
    }

    fn new_game_job(&self, kind: JobKind, game: GameRef, name: String) -> u64 {
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let job = Job {
            id,
            kind,
            game,
            name,
            status: "Queued".into(),
            downloaded: 0,
            total: 0,
            done: false,
            error: None,
            detail: Vec::new(),
        };
        self.jobs.lock().unwrap().insert(id, job.clone());
        self.emit(Event::Job(job));
        id
    }

    fn product_name(&self, product_id: u32) -> String {
        Games::load()
            .ok()
            .and_then(|g| g.get(product_id).map(|g| g.name.clone()))
            .unwrap_or_else(|| product_id.to_string())
    }

    /// Signed-in accounts for `Status`, and the default's name.
    fn account_list(&self) -> (Vec<Account>, Option<String>) {
        match store::accounts_and_default() {
            Ok((accounts, default)) => {
                let list: Vec<Account> = accounts
                    .into_iter()
                    .map(|a| Account { default: Some(&a.user_id) == default.as_ref(), user_id: a.user_id, name: a.name })
                    .collect();
                let name = list.iter().find(|a| a.default).map(|a| a.name.clone());
                (list, name)
            }
            Err(e) => {
                tracing::warn!("reading signed-in accounts: {e:#}");
                (Vec::new(), None)
            }
        }
    }

    /// Tell front ends the accounts changed (`signed_in` is the default's name).
    fn emit_accounts(&self) {
        let signed_in = self.account_list().1;
        self.emit(Event::SessionChanged { signed_in });
    }

    /// The account's name for activity lines, when more than one account is signed in.
    fn as_account(&self, online: &Online) -> String {
        match store::accounts() {
            Ok(list) if list.len() > 1 => list
                .into_iter()
                .find(|a| Some(a.user_id.as_str()) == online.user_id())
                .map(|a| format!(" as {}", a.name))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// The account to use for a game ([`launch_account`]); `explicit` (user id or name)
    /// overrides the game's setting.
    async fn account_for(&self, product_id: u32, explicit: Option<&str>) -> Result<Arc<Online>> {
        let setting = match explicit {
            Some(q) => Some(store::resolve(q)?.user_id),
            None => settings::current(product_id)?.0.account,
        };
        let (accounts, default) = store::accounts_and_default()?;
        if let Some(id) = &setting
            && !accounts.iter().any(|a| &a.user_id == id)
        {
            bail!(
                "{}'s launch account ({id}) is not signed in; sign it in, or clear it with `quiver set {product_id} account=`",
                self.product_name(product_id)
            );
        }
        // Ownership only matters when there is a choice to make.
        let owners = if setting.is_none() && accounts.len() > 1 {
            match self.owned_library(false).await {
                // If the default's library couldn't be fetched, its ownership is unknown:
                // keep the default rather than silently switching save histories.
                Ok(_) if default.as_ref().is_some_and(|d| self.library_failed.lock().unwrap().contains(d)) => Vec::new(),
                Ok(lib) => lib.into_iter().find(|e| e.ubisoft_id() == Some(product_id)).map(|e| e.owned_by).unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(product_id, "library unavailable to pick an account; using the default: {e:#}");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let id = launch_account(setting.as_deref(), default.as_deref(), &owners);
        self.accounts.get(id.as_deref())
    }

    /// The library as front ends see it: [`Self::owned_library`]'s installable games plus
    /// Steam installs (see `steamlib`).
    async fn library(self: &Arc<Self>, refresh: bool) -> Result<Vec<LibraryEntry>> {
        steamlib::library(self, refresh).await
    }

    /// Every owned game on the signed-in accounts (cached), including those Ubisoft has no
    /// PC build of (`installable: false`: copies bought on Steam).
    async fn owned_library(&self, refresh: bool) -> Result<Vec<LibraryEntry>> {
        let mut cache = self.library.lock().await;
        if refresh || cache.is_none() {
            // Default account first, so its library supplies names, art and update state.
            let onlines = self.accounts.all()?;
            if onlines.is_empty() {
                bail!("not signed in (run `quiver login`)");
            }
            let entries = run_local(move || async move {
                let installed = Games::load()?;
                let mut out: Vec<LibraryEntry> = Vec::new();
                let mut seen: HashMap<u32, usize> = HashMap::new();
                let mut first_err = None;
                let mut failed = Vec::new();
                for online in &onlines {
                    let user_id = online.user_id().unwrap_or_default().to_owned();
                    let lib = match async { Library::fetch(&online.demux().await?).await }.await {
                        Ok(lib) => lib,
                        Err(e) => {
                            tracing::warn!(account = %user_id, "library fetch failed: {e:#}");
                            first_err.get_or_insert(e);
                            failed.push(user_id);
                            continue;
                        }
                    };
                    for p in lib.products.iter().filter(|p| steamlib::excluded(p).is_none()) {
                        if let Some(&i) = seen.get(&p.product_id) {
                            let e: &mut LibraryEntry = &mut out[i];
                            e.owned_by.push(user_id.clone());
                            // Another account may own the downloadable edition.
                            e.installable |= p.is_installable();
                            continue;
                        }
                        let game = installed.get(p.product_id).cloned();
                        let update = game.as_ref().is_some_and(|g| launcher::update_available(g, &lib));
                        seen.insert(p.product_id, out.len());
                        out.push(steamlib::entry_for(p, game, update, &user_id));
                    }
                }
                if failed.len() == onlines.len() {
                    return Err(first_err.unwrap_or_else(|| anyhow::anyhow!("no library")));
                }
                out.sort_by(|a, b| b.installed.is_some().cmp(&a.installed.is_some()).then_with(|| a.name.cmp(&b.name)));
                Ok((out, failed))
            })
            .await?;
            let (entries, failed) = entries;
            *self.library_failed.lock().unwrap() = failed;
            *cache = Some(entries);
        }
        let running = self.running.lock().unwrap().clone();
        Ok(cache
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|mut e| {
                e.running = e.ubisoft_id().is_some_and(|id| running.contains(&id));
                e
            })
            .collect())
    }

    async fn handle(self: &Arc<Self>, source: &str, req: Request) -> Result<Value> {
        Ok(match req {
            Request::Status => {
                let jobs = self.jobs.lock().unwrap().values().cloned().collect();
                let (accounts, signed_in) = self.account_list();
                json!(Status {
                    version: env!("CARGO_PKG_VERSION").into(),
                    signed_in,
                    awaiting_2fa: self.pending_2fa.lock().unwrap().is_some(),
                    running: self.running.lock().unwrap().iter().copied().collect(),
                    jobs,
                    proton_default: self.cfg.proton.file_name().map(|n| n.to_string_lossy().into_owned()),
                    broker: self.broker.clone(),
                    accounts,
                })
            }
            Request::Login { email, password } => {
                let auth = self.auth.clone();
                let outcome = run_local(move || async move { auth.login(&email, &password).await }).await?;
                json!(self.finish_login(source, outcome).await?)
            }
            Request::Login2fa { code } => {
                let challenge = self.pending_2fa.lock().unwrap().clone().context("no sign-in waiting for a 2FA code")?;
                let auth = self.auth.clone();
                let session = run_local(move || async move { auth.complete_2fa(&challenge, &code).await }).await?;
                json!(self.finish_login(source, LoginOutcome::Done(session)).await?)
            }
            Request::Logout { account } => {
                let id = match account {
                    Some(q) => store::resolve(&q)?.user_id,
                    None => store::default_account()?.context("not signed in")?,
                };
                if let Some(s) = self.sessions.list().into_iter().find(|s| s.account == id) {
                    bail!("{} is running on this account; stop it first", self.product_name(s.product_id));
                }
                let removed = store::clear(Some(&id))?;
                self.accounts.reset(&id).await;
                self.broker_svc.forget(&id).await;
                *self.library.lock().await = None;
                let name = removed.map(|a| a.name).unwrap_or(id);
                self.log_activity(source, format!("signed out {name}"), None);
                self.emit_accounts();
                self.emit(Event::LibraryChanged);
                Value::Null
            }
            Request::AccountDefault { account } => {
                let a = store::resolve(&account)?;
                store::set_default(&a.user_id)?;
                if let Some(lib) = self.library.lock().await.as_mut() {
                    let accounts = store::accounts()?;
                    for e in lib.iter_mut() {
                        order_owners(&mut e.owned_by, &accounts, Some(&a.user_id));
                    }
                }
                self.log_activity(source, format!("default account is now {}", a.name), None);
                self.emit_accounts();
                self.emit(Event::LibraryChanged);
                Value::Null
            }
            Request::Library { refresh, all_stores: false } => json!(self.library(refresh).await?),
            Request::Library { refresh, all_stores: true } => json!(self.library_all(refresh).await?),
            Request::Epic(r) => epic::handle(self, source, r).await?,
            Request::Install { game, after, .. } if !game.is_ubisoft() => json!(epic::install(self, source, game.0, after).await?),
            Request::Install { game, language, after } => json!(downloads::install(self, source, downloads::ubisoft(game.0)?, language, after).await?),
            Request::Play { game, profile } => json!(self.play(source, game.0, profile).await?),
            Request::Import { game, path, .. } if !game.is_ubisoft() => json!(epic::adopt(self, source, game.0, path).await?),
            Request::Import { game, path, language } => {
                let product_id = downloads::ubisoft(game.0)?;
                let online = self.account_for(product_id, None).await?;
                json!(self.start_import(source, product_id, path, language, online))
            }
            Request::Uninstall { game, remove_prefix } if !game.is_ubisoft() => json!(epic::uninstall(self, source, game.0, remove_prefix).await?),
            Request::Uninstall { game, remove_prefix } => {
                let product_id = downloads::ubisoft(game.0)?;
                if self.running.lock().unwrap().contains(&product_id) {
                    bail!("stop the game first");
                }
                self.downloads.ensure_idle(game.0)?;
                let gref = game.0;
                let game = launcher::uninstall(product_id, remove_prefix)?;
                let exports_removed = exports::remove_game_exports(gref);
                *self.library.lock().await = None;
                self.log_activity(
                    source,
                    format!(
                        "uninstalled {}{}{}",
                        game.name,
                        if remove_prefix { " (and its compatibility folder)" } else { "" },
                        exports::removed_note(&exports_removed)
                    ),
                    Some(product_id),
                );
                self.emit(Event::LibraryChanged);
                json!(quiver_api::UninstallResult {
                    name: game.name,
                    dir: game.install_dir,
                    prefix: game.prefix,
                    prefix_removed: remove_prefix,
                    exports_removed,
                })
            }
            Request::Cancel { job_id, discard } => {
                let job = self.jobs.lock().unwrap().get(&job_id).cloned().context("no such job")?;
                if job.done {
                    bail!("job {job_id} already finished");
                }
                match job.kind {
                    JobKind::Install | JobKind::Verify => downloads::cancel(self, source, &job, discard)?,
                    JobKind::Play => {
                        let id = job.game.as_ubisoft().context("its store's client runs it; stop it there")?;
                        let game = Games::load()?.get(id).cloned().context("not installed")?;
                        launcher::stop(&self.cfg, &game).await?;
                    }
                    JobKind::Import => bail!("imports can't be cancelled"),
                }
                self.log_activity(source, format!("cancelled job {job_id} ({}){}", job.name, if discard { ", discarding files" } else { "" }), job.game.as_ubisoft());
                Value::Null
            }
            Request::Stop { game } if epic::is_installed(self, game.0) => {
                epic::stop(self, game.0).await?;
                Value::Null
            }
            Request::Stop { game } => {
                if !game.is_ubisoft() {
                    stores::stop_elsewhere(game.0)?;
                }
                let product_id = ubisoft_only(game.0)?;
                let game = Games::load()?.get(product_id).cloned().context("not installed")?;
                launcher::stop(&self.cfg, &game).await?;
                self.log_activity(source, format!("stopped {}", game.name), Some(product_id));
                Value::Null
            }
            Request::SettingsGet { game } => {
                let game = settings_game(*game)?;
                let (s, revision) = settings::current(game)?;
                json!({ "revision": revision, "settings": s })
            }
            Request::SettingsSet { game, ops, message } => {
                let game = settings_game(*game)?;
                let Some(product_id) = game.as_ubisoft() else {
                    return self.steam_settings_set(source, game, ops, message);
                };
                // `account=<name|id|prefix>` is stored as the account's user id, and
                // `profile=<name|id>` (the default profile) as the profile's id.
                let ops = ops
                    .into_iter()
                    .map(|op| match op {
                        SettingOp::Set { key, value } if key == "account" => {
                            Ok(SettingOp::Set { key, value: store::resolve(&value)?.user_id })
                        }
                        SettingOp::Set { key, value } if key == "profile" || key == "steam_profile" => {
                            Ok(SettingOp::Set { key, value: mods::resolve_default(product_id, &value)? })
                        }
                        op => Ok(op),
                    })
                    .collect::<Result<Vec<_>>>()?;
                if let Some(p) = ops.iter().find_map(|op| match op {
                    quiver_api::SettingOp::Set { key, value } if key == "proton" => Some(value.clone()),
                    _ => None,
                }) {
                    launcher::resolve_proton(&p)?;
                }
                let rev = settings::apply(product_id, &ops, source, message)?;
                self.log_activity(source, format!("settings r{} for {}", rev.revision, self.product_name(product_id)), Some(product_id));
                self.emit(Event::SettingsChanged { product_id, revision: rev.revision, source: source.to_owned() });
                json!(rev)
            }
            Request::SettingsHistory { game } => json!(settings::history(settings_game(*game)?)?),
            Request::SettingsRestore { game, revision } => {
                let game = settings_game(*game)?;
                let Some(product_id) = game.as_ubisoft() else {
                    let rev = settings::restore(game, revision, source)?;
                    self.log_activity_game(source, format!("restored settings r{revision} as r{} for {game}", rev.revision), Some(game));
                    return Ok(json!(rev));
                };
                let rev = settings::restore(product_id, revision, source)?;
                self.log_activity(source, format!("restored settings r{revision} as r{} for {}", rev.revision, self.product_name(product_id)), Some(product_id));
                self.emit(Event::SettingsChanged { product_id, revision: rev.revision, source: source.to_owned() });
                json!(rev)
            }
            Request::LaunchContext { product_id, savegame_path, install_dir, account, steam_app_id } => {
                let online = self.account_for(product_id, account.as_deref()).await?;
                if let Some(app_id) = steam_app_id {
                    self.sessions.from_steam(product_id, app_id);
                }
                let label = self.as_account(&online);
                let broker = self.broker.clone();
                let ctx = run_local(move || async move {
                    let session = online.session().await?;
                    let demux = online.demux().await?;
                    let mut lib = Library::fetch(&demux).await?;
                    let mut ctx = quiver_core::launch::build(&online.auth, &session, &mut lib, product_id).await?;
                    ctx.savegame_path = savegame_path;
                    ctx.broker = Some(broker);
                    if let Some(dir) = install_dir {
                        ctx.chunks = quiver_core::launch::installed_chunks(&dir).unwrap_or_default();
                    }
                    Ok(ctx)
                })
                .await?;
                self.log_activity(source, format!("issued launch context for {product_id}{label}"), Some(product_id));
                json!(ctx)
            }
            Request::ProtonList => json!(launcher::list_protons()),
            Request::Playtime { game } => json!(self.playtime(game.0).await?),
            Request::RemotePlaytime { product_id } => {
                let online = self.account_for(product_id, None).await?;
                run_local(move || async move {
                    use quiver_core::proto::playtime as pt;
                    let session = online.session().await?;
                    let demux = online.demux().await?;
                    let mut conn = demux.open::<quiver_core::demux::Playtime>().await?;
                    let rsp = conn
                        .request(pt::Req {
                            get_playtime_req: Some(pt::GetPlaytimeReq {
                                account_id: Some(session.user_id.clone()),
                                product_id: Some(product_id),
                            }),
                            ..Default::default()
                        })
                        .await?;
                    let r = rsp.get_playtime_rsp.context("no get_playtime_rsp")?;
                    Ok(json!({ "result": r.result, "seconds": r.seconds, "last_played": r.last_played }))
                })
                .await?
            }
            Request::Activity { limit } => {
                let a = self.activity.lock().unwrap();
                json!(a.iter().rev().take(limit).cloned().collect::<Vec<_>>())
            }
            // With the options addressing fields (C1); older clients ignore them.
            // Then Steam games played through `quiver steam-run` (with `game`).
            Request::Sessions => json!(self.sessions.list().into_iter().map(|s| self.options.detail(s)).chain(self.local.list()).collect::<Vec<_>>()),
            r @ (Request::ExportList | Request::ExportDesktop { .. } | Request::ExportSteam { .. } | Request::SteamUsers | Request::SteamApplyStaged | Request::SteamRestart { .. }) => exports::handle(self, source, r).await?,
            r @ (Request::GameInfo { .. } | Request::GameProgress { .. } | Request::Saves { .. } | Request::SavesRestore { .. } | Request::SavesSync { .. }) => info::handle(self, source, r).await?,
            r @ (Request::Queue
            | Request::DownloadThroughput { .. }
            | Request::QueueMove { .. }
            | Request::QueueRunNow { .. }
            | Request::QueueSetAfter { .. }
            | Request::QueuePause { .. }
            | Request::QueueResume { .. }
            | Request::QueueRemove { .. }
            | Request::InstallPreview { .. }
            | Request::UpdatePreview { .. }
            | Request::Update { .. }
            | Request::Verify { .. }
            | Request::VerifyResult { .. }
            | Request::DownloadSettings { .. }) => downloads::handle(self, source, r).await?,
            r @ (Request::Setup { .. } | Request::ScanInstalls) => setup::handle(self, source, r).await?,
            r @ (Request::SteamGames { .. }
            | Request::SteamPatch { .. }
            | Request::SteamUnpatch { .. }
            | Request::SteamDismissOffer { .. }
            | Request::SteamOffers { .. }) => steamgames::handle(self, source, r).await?,
            r @ (Request::SteamHook { .. } | Request::SteamHookStatus { .. }) => steamhook::handle(self, source, r).await?,
            Request::SteamSession { app_id, pid, perf_session, settings_revision, mods_launch } => {
                json!(self.steam_session(source, app_id, pid, perf_session, settings_revision, mods_launch).await?)
            }
            r @ (Request::Ownership { .. } | Request::SteamLaunchPlan { .. }) => steamlib::handle(self, source, r).await?,
            Request::Subscribe => Value::Null, // handled by the connection loop
            Request::Shutdown => {
                if !self.running.lock().unwrap().is_empty() {
                    bail!("games are running; stop them first");
                }
                tracing::info!("shutdown requested by {source}");
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = std::fs::remove_file(quiver_api::socket_path());
                    std::process::exit(0);
                });
                Value::Null
            }
            Request::Mods(req) => self.handle_mods(source, req).await?,
            Request::Options(req) => self.handle_options(source, req).await?,
            Request::Tweaks(req) => self.handle_tweaks(source, req).await?,
            Request::ModSdk(quiver_api::mods::ModSdkRequest::Status { product_id, game }) => match game {
                Some(g) => json!(self.mod_sdk.connections_of(g)),
                None => json!(self.mod_sdk.connections(product_id)),
            },
            // The connection loop passes connection-bound perf requests (subscriptions, HUD
            // attachment) with their connection; this path has none.
            Request::Perf(req) => self.handle_perf(source, None, req).await?,
            Request::Sight(req) => sightinfo::handle(self, source, req).await?,
            Request::LaunchMaterial { game, session, savegame_path, install_dir, ovt_dir, account } => {
                let r = lab::MaterialRequest { game, session, savegame_path, install_dir, ovt_dir, account };
                lab::launch_material(self, source, r).await?
            }
            Request::Achievements(req) => achievements::handle(self, source, req).await?,
            // The connection loop passes its connection (holds without a time limit).
            Request::Daemon(req) => self.handle_daemon(source, None, req)?,
            Request::Stores => json!(self.store_list().await?),
            Request::GameDetail { game, refresh } => json!(self.game_detail(game, refresh).await?),
            r @ (Request::SaveStores { .. } | Request::SaveStoreEdit { .. } | Request::SaveStoreHistory { .. } | Request::SavesMerge { .. }) => {
                savestores::handle(self, source, r).await?
            }
        })
    }

    /// A sign-in adds its account (or refreshes it, if already signed in); the first
    /// account becomes the default.
    async fn finish_login(&self, source: &str, outcome: LoginOutcome) -> Result<LoginResult> {
        match outcome {
            LoginOutcome::TwoFactor(c) => {
                *self.pending_2fa.lock().unwrap() = Some(c);
                *self.pending_2fa_at.lock().unwrap() = Some(std::time::Instant::now());
                Ok(LoginResult::TwoFactorRequired)
            }
            LoginOutcome::Done(session) => {
                let known = store::accounts()?.iter().any(|a| a.user_id == session.user_id);
                store::save(&session)?;
                *self.pending_2fa.lock().unwrap() = None;
                // Reconnect with the new ticket.
                self.accounts.reset(&session.user_id).await;
                self.broker_svc.forget(&session.user_id).await;
                *self.library.lock().await = None;
                let what = if known { "signed in again as" } else { "added account" };
                self.log_activity(source, format!("{what} {}", session.name), None);
                self.emit_accounts();
                self.emit(Event::LibraryChanged);
                Ok(LoginResult::SignedIn { name: session.name })
            }
        }
    }

    fn start_import(
        self: &Arc<Self>,
        source: &str,
        product_id: u32,
        path: std::path::PathBuf,
        language: Option<String>,
        online: Arc<Online>,
    ) -> u64 {
        let id = self.new_job(JobKind::Import, product_id, product_id.to_string());
        self.log_activity(source, format!("import {product_id} from {}{}", path.display(), self.as_account(&online)), Some(product_id));
        let this = self.clone();
        tokio::spawn(async move {
            let cfg = this.cfg.clone();
            let lang = language.unwrap_or_else(|| "en-US".into());
            let st = this.clone();
            let result = run_local(move || async move {
                launcher::import_game(&cfg, &online, product_id, &path, &lang, |s| st.update_job(id, |j| j.status = s)).await
            })
            .await;
            *this.library.lock().await = None;
            this.update_job(id, |j| {
                j.done = true;
                match &result {
                    Ok(r) => {
                        j.name = r.game.name.clone();
                        j.status = format!(
                            "Imported {} files{}",
                            r.files,
                            if r.update_available { "; an update is available" } else { "" }
                        );
                        j.detail = r.extras.clone();
                    }
                    Err(e) => {
                        j.status = "Failed".into();
                        j.error = Some(format!("{e:#}"));
                    }
                }
            });
            this.emit(Event::LibraryChanged);
        });
        id
    }

    fn start_play(self: &Arc<Self>, source: &str, product_id: u32, profile: Option<String>, online: Arc<Online>) -> Result<u64> {
        let game = Games::load()?.get(product_id).cloned().context("not installed")?;
        // The launch plan (Mods v2): the profile's mod stack (or vanilla, with fixes if
        // that's on), settings resolved through it, mounts, saves and config patches.
        // No profile named: the game's default profile (vanilla if it was deleted, with a
        // warning in the activity log and the job).
        let (mut plan, fallback) = mods::launch_plan(&game, profile.as_deref(), source)?;
        if let Some(w) = fallback {
            self.log_activity(source, format!("{}: {w}", game.name), Some(product_id));
        }
        if !plan.api.problems.is_empty() {
            bail!("can't launch {}: {}", game.name, plan.api.problems.join("; "));
        }
        let (_, rev) = settings::current(product_id)?;
        if !self.running.lock().unwrap().insert(product_id) {
            bail!("{} is already running", game.name);
        }
        let id = self.new_job(JobKind::Play, product_id, game.name.clone());
        // Options (design/MOD-CONFIG.md): the session id the game and its mods see
        // (`QUIVER_SESSION`), and the profile's option files rendered and patched.
        let session_id = options::new_session_id();
        plan.session = Some(session_id.clone());
        plan.game = Some(quiver_api::GameRef::ubisoft(product_id));
        let mut detail = mods::plan_lines(&plan.api);
        detail.extend(self.options_launch(&game, &mut plan, &session_id));
        // Perf (design/SIGHT-HUD.md): the session's metrics, and with the HUD enabled for
        // the game, its session dir, the Sight layer's environment and the MangoHud remap.
        let perf_profile = plan.api.profile.as_ref().map(|p| p.id.clone());
        // Quiver Tweaks (design/TWEAKS.md): the compiled environment, before the perf part so
        // the layer order sees variables tweaks turn on (NVIDIA Smooth Motion).
        let (tweak_lines, tweak_revisions) = self.tweaks_launch(&game, &mut plan, &session_id);
        detail.extend(tweak_lines);
        detail.extend(self.perf_launch(&session_id, &game, perf_profile.as_deref(), &mut plan.api.settings));
        if !detail.is_empty() {
            self.update_job(id, |j| j.detail = detail);
        }
        let with = match &plan.api.profile {
            Some(p) => format!(
                ", {}profile '{}' ({}) r{}",
                if profile.is_none() { "default " } else { "" },
                p.name,
                p.id,
                plan.api.revision
            ),
            None if !plan.api.mods.is_empty() => format!(", fixes {}", mods::mods_line(&plan.api)),
            None => String::new(),
        };
        // Which version of each mod, so the activity log says what really ran.
        let with = match &plan.api.profile {
            Some(_) if !plan.api.resolved.is_empty() => format!("{with}: {}", mods::mods_line(&plan.api)),
            _ => with,
        };
        self.log_activity(source, format!("play {}{} (settings r{rev}{with})", game.name, self.as_account(&online)), Some(product_id));
        let this = self.clone();
        tokio::spawn(async move {
            let (cfg, broker) = (this.cfg.clone(), this.broker.clone());
            let sessions = this.sessions.clone();
            let ev_this = this.clone();
            let g = game.clone();
            let started_at: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>> = Arc::default();
            let started = started_at.clone();
            let (profile_id, profile_revision) =
                (plan.api.profile.as_ref().map(|p| p.id.clone()), plan.api.profile.as_ref().map(|_| plan.api.revision));
            let session_mods = plan.api.resolved.clone();
            let options_revision = this.options.launched_revision(&session_id);
            let exit_code: Arc<Mutex<Option<i32>>> = Arc::default();
            let exit = exit_code.clone();
            let sid = session_id.clone();
            this.options_watch(session_id.clone());
            this.tweaks_watch(session_id.clone());
            let result = run_local(move || async move {
                launcher::play(&cfg, &online, &g, &plan, &broker, &sessions, |e| match e {
                    LaunchEvent::Detail(lines) => ev_this.update_job(id, |j| j.detail.extend(lines)),
                    LaunchEvent::Status(msg) => ev_this.update_job(id, |j| j.status = msg),
                    LaunchEvent::Running => {
                        *started.lock().unwrap() = Some(chrono::Utc::now());
                        ev_this.update_job(id, |j| j.status = "Running".into());
                        ev_this.emit(Event::GameStarted { product_id });
                    }
                    LaunchEvent::Exited(code) => {
                        *exit.lock().unwrap() = code;
                        ev_this.update_job(id, |j| j.status = "Game exited".into())
                    }
                    LaunchEvent::Pids(pids) => {
                        ev_this.perf.set_pids(&sid, pids.clone());
                        ev_this.options_pids(&sid, pids)
                    }
                })
                .await
            })
            .await;
            this.running.lock().unwrap().remove(&product_id);
            let code = *exit_code.lock().unwrap();
            this.options_session_end(&session_id, code).await;
            this.tweaks_session_end(&session_id, product_id);
            let perf_summary = this.perf.end(&session_id);
            if let Some(start) = *started_at.lock().unwrap() {
                let end = chrono::Utc::now();
                let session = quiver_api::PlaySession {
                    started: start,
                    ended: end,
                    seconds: (end - start).num_seconds().max(0) as u64,
                    settings_revision: rev,
                    profile: profile_id.clone(),
                    profile_revision,
                    mods: session_mods,
                    session: Some(session_id.clone()),
                    options_revision,
                    // Perf: the session's summary (`PerfRequest::Summaries` reads it back).
                    perf: perf_summary,
                    tweaks: tweak_revisions,
                };
                if let Err(e) = settings::record_session(product_id, &session) {
                    tracing::warn!("recording play session: {e:#}");
                }
            }
            this.emit(Event::GameStopped { product_id });
            this.update_job(id, |j| {
                j.done = true;
                if let Err(e) = &result {
                    j.status = "Failed".into();
                    j.error = Some(format!("{e:#}"));
                }
            });
        });
        Ok(id)
    }
}

/// A session no quiverd launch held, started from Steam by `quiver steam-run`, into the
/// game's local play history, with the settings revision it ran
/// under (Steam launches apply the game's settings; see `steamlib::launch_plan`).
/// `perf`: the summary of its perf session (`quiver steam-run` asks for one).
fn record_bridge_session(f: &quiver_core::playsession::Finished, perf: Option<quiver_api::perf::PerfSummary>) {
    // Only Steam launches through `quiver steam-run` are someone playing. Other bridges
    // (containers, agents' test instances, often several at once and dozens a day) would
    // bury the person's own play history, so they stay out of it.
    let Some(app) = f.steam_app_id else {
        tracing::debug!(product_id = f.product_id, holders = ?f.holders, "not recording a bridge session that Steam didn't start");
        return;
    };
    let rev = settings::current(f.product_id).map(|(_, r)| r).unwrap_or(0);
    let session = quiver_api::PlaySession {
        started: f.started,
        ended: f.ended,
        seconds: (f.ended - f.started).num_seconds().max(0) as u64,
        settings_revision: rev,
        profile: None,
        profile_revision: None,
        // Steam launches don't apply mod profiles (yet).
        mods: Vec::new(),
        // Perf: as quiverd's own launches record it (`PerfRequest::Summaries` reads it too).
        perf,
        ..Default::default()
    };
    let via = format!("Steam app {app}");
    tracing::info!(product_id = f.product_id, seconds = session.seconds, "recording a play session held by {via}");
    if let Err(e) = settings::record_session(f.product_id, &session) {
        tracing::warn!("recording play session: {e:#}");
    }
}

/// Connection ids, for per-connection perf state and holds.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// The method a request counts under while in flight (`None`: quick and read-only; it
/// never holds up a restart and is answered during one).
fn gated(req: &Request) -> Option<String> {
    match req {
        Request::Status
        | Request::Subscribe
        | Request::Sessions
        | Request::Activity { .. }
        | Request::Daemon(quiver_api::daemon::DaemonRequest::Status)
        | Request::Sight(quiver_api::sight::SightRequest::Status) => None,
        r => Some(
            serde_json::to_value(r)
                .ok()
                .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_owned))
                .unwrap_or_else(|| "request".into()),
        ),
    }
}

async fn serve_conn(daemon: Arc<Daemon>, stream: UnixStream) {
    let conn_id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Reply>();
    let closing_written = Arc::new(tokio::sync::Notify::new());
    daemon.conns.lock().unwrap().insert(conn_id, (tx.clone(), closing_written.clone()));
    let writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            let closing = matches!(reply, Reply::Closing { .. });
            // Options and perf events go out as `Event`s (clients read the older forms too).
            let mut line = serde_json::to_vec(&reply.folded()).unwrap_or_default();
            line.push(b'\n');
            let ok = w.write_all(&line).await.is_ok() && w.flush().await.is_ok();
            if closing {
                closing_written.notify_one();
            }
            if !ok {
                break;
            }
        }
    });
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let env: Envelope = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Reply::Err { id: 0, error: format!("bad request: {e}"), code: None });
                continue;
            }
        };
        if matches!(env.request, Request::Subscribe) {
            let mut perf_events = daemon.perf.broadcast.subscribe();
            let tx4 = tx.clone();
            tokio::spawn(async move {
                loop {
                    match perf_events.recv().await {
                        Ok(e) => {
                            if tx4.send(Reply::Perf(e)).is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            let mut options = daemon.options.events.subscribe();
            let tx3 = tx.clone();
            tokio::spawn(async move {
                loop {
                    match options.recv().await {
                        Ok(e) => {
                            if tx3.send(Reply::Options(e)).is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
            let mut events = daemon.events.subscribe();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(e) => {
                            if tx2.send(Reply::Event(e)).is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }
        // While restarting onto a new build, requests are refused (not run) with a code
        // that tells clients to send them again; the rest count as in flight until answered.
        let pass = match daemon.lifecycle.enter(gated(&env.request).as_deref()) {
            Ok(p) => p,
            Err(error) => {
                let _ = tx.send(Reply::Err { id: env.id, error, code: Some(quiver_api::daemon::RESTARTING.into()) });
                continue;
            }
        };
        let daemon = daemon.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let _pass = pass;
            let result = match env.request {
                // Perf subscriptions and HUD attachments belong to this connection.
                Request::Perf(req) => daemon.handle_perf(&env.source, Some(perf::ConnCtx { id: conn_id, tx: tx.clone() }), req).await,
                // So do holds without a time limit.
                Request::Daemon(req) => daemon.handle_daemon(&env.source, Some(conn_id), req),
                req => daemon.handle(&env.source, req).await,
            };
            let reply = match result {
                Ok(result) => Reply::Ok { id: env.id, result },
                Err(e) => Reply::Err { id: env.id, error: format!("{e:#}"), code: None },
            };
            let _ = tx.send(reply);
        });
    }
    daemon.perf.disconnect(conn_id);
    daemon.lifecycle_disconnect(conn_id);
    daemon.conns.lock().unwrap().remove(&conn_id);
    drop(tx);
    let _ = writer.await;
}

#[tokio::main]
async fn main() -> Result<()> {
    // No options: any argument is for a person, so never start the daemon for one.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => println!("quiverd {}", env!("CARGO_PKG_VERSION")),
            "-h" | "--help" => println!(
                "quiverd: Quiver's daemon. Takes no arguments; clients (quiver, quiver-app) start it on demand.\n\
                 Socket: {}\nStop it with: quiver daemon stop",
                quiver_api::socket_path().display()
            ),
            _ => bail!("quiverd takes no arguments (try --help)"),
        }
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "quiverd=info,quiver_core=info,epic_core=info".into()),
        )
        .init();
    // After a restart onto a new build: the previous build's listening sockets.
    let inherited = lifecycle::take_inherited();
    let lifecycle = lifecycle::Lifecycle::from_env();
    let path = quiver_api::socket_path();
    let listener = match inherited.unix {
        Some(l) => {
            l.set_nonblocking(true)?;
            tracing::info!(
                "quiverd {} restarted onto this build (was {})",
                lifecycle::build(),
                lifecycle.restarted_from.as_deref().unwrap_or("?")
            );
            UnixListener::from_std(l)?
        }
        None => {
            if UnixStream::connect(&path).await.is_ok() {
                bail!("quiverd is already running ({})", path.display());
            }
            let dir = path.parent().context("socket path")?;
            std::fs::create_dir_all(dir)?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
            let _ = std::fs::remove_file(&path);
            UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?
        }
    };

    let cfg = Arc::new(Config::load()?);
    let accounts = Accounts::new();
    // A fixed port lets external bridges (containers, Steam prefixes) find the broker.
    let broker_addr = std::env::var("QUIVER_BROKER")
        .or_else(|_| std::env::var("UBID_BROKER"))
        .unwrap_or_else(|_| "127.0.0.1:47810".into());
    let broker_listener = match inherited.broker {
        Some(l) => {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)?
        }
        None => match tokio::net::TcpListener::bind(&broker_addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("broker port {broker_addr} unavailable ({e}); using an ephemeral port");
                tokio::net::TcpListener::bind("127.0.0.1:0").await?
            }
        },
    };
    let broker = broker_listener.local_addr()?.to_string();
    let (events, _) = broadcast::channel(256);
    // The perf hub takes mods' metrics, and ends Steam launches' perf sessions with their
    // play sessions.
    let perf = perf::PerfHub::start(launcher::data_dir(), perf::shm_root());
    let local = Arc::new(localsessions::LocalSessions::default());
    let sessions = {
        let finished_perf = perf.clone();
        let events = events.clone();
        let list: Arc<Mutex<Option<Arc<PlaySessions>>>> = Arc::default();
        let l = list.clone();
        let local = local.clone();
        let finished_events = events.clone();
        let s = PlaySessions::new(
            accounts.clone(),
            cfg.report_playtime,
            move || {
                if let Some(s) = &*l.lock().unwrap() {
                    let mut sessions = s.list();
                    sessions.extend(local.list());
                    let _ = events.send(Event::SessionsChanged { sessions });
                }
            },
            move |f| {
                let summary = f.steam_app_id.and_then(|_| finished_perf.end_steam(&GameRef::ubisoft(f.product_id)));
                record_bridge_session(&f, summary);
                let _ = finished_events.send(Event::GameStopped { product_id: f.product_id });
            },
        );
        *list.lock().unwrap() = Some(s.clone());
        s
    };
    let broker_svc = Broker::new(accounts.clone(), Some(sessions.clone()));
    // Stand-in sinks until the Options backend and the perf hub install theirs
    // (`set_options_sink`, `set_metrics_sink`).
    let mod_sdk = quiver_core::modsdk::ModRegistry::logging();
    mod_sdk.set_metrics_sink(perf::mods::PerfModSink::new(&perf));
    broker_svc.set_mods(mod_sdk.clone());
    let ach = achievements::engine(accounts.clone(), events.clone());
    broker_svc.set_achievements(ach.clone());

    // Also moves a pre-multi-account keyring entry to its per-account one.
    match store::accounts_and_default() {
        Ok((list, _)) if list.is_empty() => tracing::info!("not signed in"),
        Ok((list, default)) => {
            let names: Vec<String> = list
                .iter()
                .map(|a| if Some(&a.user_id) == default.as_ref() { format!("{} (default)", a.name) } else { a.name.clone() })
                .collect();
            tracing::info!("signed in: {}", names.join(", "));
        }
        Err(e) => tracing::warn!("reading signed-in accounts: {e:#}"),
    }
    let activity: VecDeque<ActivityEntry> = std::fs::read_to_string(launcher::data_dir().join("activity.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(ACTIVITY_KEEP)
        .rev()
        .collect();
    let queue = downloads::load(&cfg);
    let epic = Arc::new(epic_core::Epic::open(&launcher::data_dir())?);
    let daemon = Arc::new(Daemon {
        accounts,
        auth: Arc::new(Auth::default()),
        broker_svc,
        cfg,
        broker,
        pending_2fa: Mutex::new(None),
        jobs: Mutex::new(BTreeMap::new()),
        next_job: AtomicU64::new(1),
        running: Mutex::new(HashSet::new()),
        running_games: Mutex::new(HashSet::new()),
        downloads: queue,
        library: tokio::sync::Mutex::new(None),
        library_failed: Mutex::new(Vec::new()),
        activity: Mutex::new(activity),
        events,
        sessions,
        local,
        options: options::Options::default(),
        tweaks: tweaks::Tweaks::default(),
        mod_sdk,
        perf,
        achievements: ach,
        stores: vec![
            Arc::new(quiver_core::steamlocal::SteamLocal::new(&quiver_core::exports::Paths::from_env().home)),
            Arc::new(epic_core::provider::EpicProvider::new(epic.clone())),
        ],
        epic,
        lifecycle,
        conns: Mutex::new(HashMap::new()),
        pending_2fa_at: Mutex::new(None),
    });
    options::test_session(&daemon);
    downloads::start(&daemon);
    steamgames::start(&daemon);
    achievements::start(&daemon);
    epicgame::start(&daemon);
    sightinfo::start(&daemon);
    daemon.start_perf_steam_sweep();
    let (restart_tx, mut restart_rx) = mpsc::channel::<()>(1);
    lifecycle::start_watch(&daemon, restart_tx);
    tracing::info!("quiverd {} listening on {} (broker {})", lifecycle::build(), path.display(), daemon.broker);
    // Clients and bridges are accepted here, so while a restart is being decided and
    // carried out nobody is (they wait in the backlog for whichever build serves next).
    loop {
        tokio::select! {
            r = listener.accept() => {
                let (stream, _) = r?;
                tokio::spawn(serve_conn(daemon.clone(), stream));
            }
            r = broker_listener.accept() => match r {
                Ok((stream, peer)) => {
                    let pass = daemon.lifecycle.broker_conn();
                    let broker = daemon.broker_svc.clone();
                    tokio::spawn(async move {
                        let _pass = pass;
                        broker.serve_one(stream, peer).await;
                    });
                }
                Err(e) => {
                    tracing::warn!("broker accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(()) = restart_rx.recv() => {
                let plan = match daemon.lifecycle.try_close(|| daemon.busy()) {
                    Ok(plan) => plan,
                    Err(why) => {
                        tracing::debug!("not restarting yet: {}", why.join(", "));
                        continue;
                    }
                };
                daemon.announce_restart(&plan).await;
                use std::os::fd::AsRawFd;
                tracing::info!("restarting onto {} ({})", plan.to.as_deref().unwrap_or("a new build"), plan.exe.display());
                let fds = [("unix", listener.as_raw_fd()), ("broker", broker_listener.as_raw_fd())];
                let err = lifecycle::exec(&plan, &fds, daemon.lifecycle.restarts);
                // Only back here if the exec failed: carry on with this build.
                tracing::error!("restarting onto {}: {err}", plan.exe.display());
                daemon.log_activity("system", format!("quiverd couldn't restart onto the new build ({err}); still running {}", lifecycle::build()), None);
                daemon.lifecycle.reopen(&plan, &err.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn launch_account_rule() {
        let pick = |setting, default, owners: &[&str]| launch_account(setting, default, &ids(owners));
        // The game's setting wins, even over a default that owns it.
        assert_eq!(pick(Some("fam"), Some("me"), &["me", "fam"]).as_deref(), Some("fam"));
        assert_eq!(pick(Some("fam"), Some("me"), &[]).as_deref(), Some("fam"));
        // The default, when it owns the game.
        assert_eq!(pick(None, Some("me"), &["me", "fam"]).as_deref(), Some("me"));
        assert_eq!(pick(None, Some("me"), &["fam", "me"]).as_deref(), Some("me"));
        // Otherwise the first owner.
        assert_eq!(pick(None, Some("me"), &["fam", "third"]).as_deref(), Some("fam"));
        // Ownership unknown: the default.
        assert_eq!(pick(None, Some("me"), &[]).as_deref(), Some("me"));
        // No default (nobody signed in): only an owner or a setting can name one.
        assert_eq!(pick(None, None, &[]), None);
        assert_eq!(pick(None, None, &["fam"]).as_deref(), Some("fam"));
    }

    #[test]
    fn owners_default_first_then_sign_in_order() {
        let accounts: Vec<AccountRef> = ["a", "b", "c"]
            .iter()
            .map(|id| AccountRef { user_id: id.to_string(), name: id.to_uppercase() })
            .collect();
        let mut owners = ids(&["a", "b", "c"]);
        order_owners(&mut owners, &accounts, Some("c"));
        assert_eq!(owners, ["c", "a", "b"]);
        order_owners(&mut owners, &accounts, Some("b"));
        assert_eq!(owners, ["b", "a", "c"]);
        let mut owners = ids(&["c", "x", "a"]);
        order_owners(&mut owners, &accounts, None);
        assert_eq!(owners, ["a", "c", "x"]);
    }
}

