//! ubid: the launcher's backend. Owns the Ubisoft session, the Denuvo broker, installed
//! games, per-game settings (with history), install/launch jobs and an activity log.
//! Front ends (`ubi`, the GUI, scripts and agents) talk to it over a Unix socket; see
//! `ubi-api` for the protocol.

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
use ubi_api::{
    Account, ActivityEntry, Art, Envelope, Event, Job, JobKind, LibraryEntry, LoginResult, Reply, Request, SettingOp,
    Status,
};
use ubi_core::auth::{Auth, LoginOutcome, TwoFactorChallenge};
use ubi_core::broker::Broker;
use ubi_core::install::Progress;
use ubi_core::launcher::{self, Config, Games, LaunchEvent};
use ubi_core::library::{Library, ProductConfig};
use ubi_core::online::{Accounts, Online};
use ubi_core::playsession::PlaySessions;
use ubi_core::store::{self, AccountRef};
use ubi_core::settings;

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
    /// Running installs: progress handle (to cancel), whether to discard on cancel, and the
    /// install folder.
    installs: Mutex<BTreeMap<u64, (Arc<Progress>, bool, std::path::PathBuf)>>,
    library: tokio::sync::Mutex<Option<Vec<LibraryEntry>>>,
    activity: Mutex<VecDeque<ActivityEntry>>,
    events: broadcast::Sender<Event>,
    /// Games being played, from any launch path; reports play time to Ubisoft.
    sessions: Arc<PlaySessions>,
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
        let entry = ActivityEntry { at: chrono::Utc::now(), source: source.to_owned(), action, product_id };
        tracing::info!(source, product_id, "{}", entry.action);
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
        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let job = Job {
            id,
            kind,
            product_id,
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
            match self.library(false).await {
                Ok(lib) => lib.into_iter().find(|e| e.product_id == product_id).map(|e| e.owned_by).unwrap_or_default(),
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

    async fn library(&self, refresh: bool) -> Result<Vec<LibraryEntry>> {
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
                let mut fetched = 0;
                for online in &onlines {
                    let user_id = online.user_id().unwrap_or_default().to_owned();
                    let lib = match async { Library::fetch(&online.demux().await?).await }.await {
                        Ok(lib) => lib,
                        Err(e) => {
                            tracing::warn!(account = %user_id, "library fetch failed: {e:#}");
                            first_err.get_or_insert(e);
                            continue;
                        }
                    };
                    fetched += 1;
                    for p in lib
                        .products
                        .iter()
                        .filter(|p| p.owned && p.is_installable() && p.product_type == 0 && !p.locked_by_subscription)
                    {
                        if let Some(&i) = seen.get(&p.product_id) {
                            out[i].owned_by.push(user_id.clone());
                            continue;
                        }
                        let game = installed.get(p.product_id).cloned();
                        let config = p.configuration.as_deref().and_then(|c| ProductConfig::parse(c).ok());
                        let art = config
                            .map(|c| Art {
                                thumb: c.art("thumb_image"),
                                background: c.art("background_image"),
                                logo: c.art("logo_image"),
                            })
                            .unwrap_or_default();
                        seen.insert(p.product_id, out.len());
                        out.push(LibraryEntry {
                            product_id: p.product_id,
                            name: p.name.clone().unwrap_or_else(|| p.product_id.to_string()),
                            update_available: game.as_ref().is_some_and(|g| launcher::update_available(g, &lib)),
                            installed: game,
                            running: false,
                            art,
                            owned_by: vec![user_id.clone()],
                        });
                    }
                }
                if fetched == 0 {
                    return Err(first_err.unwrap_or_else(|| anyhow::anyhow!("no library")));
                }
                out.sort_by(|a, b| b.installed.is_some().cmp(&a.installed.is_some()).then_with(|| a.name.cmp(&b.name)));
                Ok(out)
            })
            .await?;
            *cache = Some(entries);
        }
        let running = self.running.lock().unwrap().clone();
        Ok(cache
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(|mut e| {
                e.running = running.contains(&e.product_id);
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
            Request::Library { refresh } => json!(self.library(refresh).await?),
            Request::Install { product_id, language } => {
                let name = self
                    .library(false)
                    .await
                    .ok()
                    .and_then(|l| l.into_iter().find(|e| e.product_id == product_id).map(|e| e.name))
                    .unwrap_or_else(|| self.product_name(product_id));
                let online = self.account_for(product_id, None).await?;
                json!(self.start_install(source, product_id, name, language, online))
            }
            Request::Play { product_id } => {
                let online = self.account_for(product_id, None).await?;
                json!(self.start_play(source, product_id, online)?)
            }
            Request::Import { product_id, path, language } => {
                let online = self.account_for(product_id, None).await?;
                json!(self.start_import(source, product_id, path, language, online))
            }
            Request::Uninstall { product_id, remove_prefix } => {
                if self.running.lock().unwrap().contains(&product_id) {
                    bail!("stop the game first");
                }
                let game = launcher::uninstall(product_id, remove_prefix)?;
                *self.library.lock().await = None;
                self.log_activity(
                    source,
                    format!("uninstalled {}{}", game.name, if remove_prefix { " (and its prefix)" } else { "" }),
                    Some(product_id),
                );
                self.emit(Event::LibraryChanged);
                Value::Null
            }
            Request::Cancel { job_id, discard } => {
                let job = self.jobs.lock().unwrap().get(&job_id).cloned().context("no such job")?;
                if job.done {
                    bail!("job {job_id} already finished");
                }
                match job.kind {
                    JobKind::Install => {
                        let mut installs = self.installs.lock().unwrap();
                        let entry = installs.get_mut(&job_id).context("install not cancellable yet")?;
                        entry.0.cancel.store(true, Ordering::Relaxed);
                        entry.1 = discard;
                        drop(installs);
                        self.update_job(job_id, |j| j.status = "Cancelling…".into());
                    }
                    JobKind::Play => {
                        let game = Games::load()?.get(job.product_id).cloned().context("not installed")?;
                        launcher::stop(&self.cfg, &game).await?;
                    }
                    JobKind::Import => bail!("imports can't be cancelled"),
                }
                self.log_activity(source, format!("cancelled job {job_id} ({}){}", job.name, if discard { ", discarding files" } else { "" }), Some(job.product_id));
                Value::Null
            }
            Request::Stop { product_id } => {
                let game = Games::load()?.get(product_id).cloned().context("not installed")?;
                launcher::stop(&self.cfg, &game).await?;
                self.log_activity(source, format!("stopped {}", game.name), Some(product_id));
                Value::Null
            }
            Request::SettingsGet { product_id } => {
                let (s, revision) = settings::current(product_id)?;
                json!({ "revision": revision, "settings": s })
            }
            Request::SettingsSet { product_id, ops, message } => {
                // `account=<name|id|prefix>` is stored as the account's user id.
                let ops = ops
                    .into_iter()
                    .map(|op| match op {
                        SettingOp::Set { key, value } if key == "account" => {
                            Ok(SettingOp::Set { key, value: store::resolve(&value)?.user_id })
                        }
                        op => Ok(op),
                    })
                    .collect::<Result<Vec<_>>>()?;
                if let Some(p) = ops.iter().find_map(|op| match op {
                    ubi_api::SettingOp::Set { key, value } if key == "proton" => Some(value.clone()),
                    _ => None,
                }) {
                    launcher::resolve_proton(&p)?;
                }
                let rev = settings::apply(product_id, &ops, source, message)?;
                self.log_activity(source, format!("settings r{} for {}", rev.revision, self.product_name(product_id)), Some(product_id));
                self.emit(Event::SettingsChanged { product_id, revision: rev.revision, source: source.to_owned() });
                json!(rev)
            }
            Request::SettingsHistory { product_id } => json!(settings::history(product_id)?),
            Request::SettingsRestore { product_id, revision } => {
                let rev = settings::restore(product_id, revision, source)?;
                self.log_activity(source, format!("restored settings r{revision} as r{} for {}", rev.revision, self.product_name(product_id)), Some(product_id));
                self.emit(Event::SettingsChanged { product_id, revision: rev.revision, source: source.to_owned() });
                json!(rev)
            }
            Request::LaunchContext { product_id, savegame_path, install_dir, account } => {
                let online = self.account_for(product_id, account.as_deref()).await?;
                let label = self.as_account(&online);
                let broker = self.broker.clone();
                let ctx = run_local(move || async move {
                    let session = online.session().await?;
                    let demux = online.demux().await?;
                    let mut lib = Library::fetch(&demux).await?;
                    let mut ctx = ubi_core::launch::build(&online.auth, &session, &mut lib, product_id).await?;
                    ctx.savegame_path = savegame_path;
                    ctx.broker = Some(broker);
                    if let Some(dir) = install_dir {
                        ctx.chunks = ubi_core::launch::installed_chunks(&dir).unwrap_or_default();
                    }
                    Ok(ctx)
                })
                .await?;
                self.log_activity(source, format!("issued launch context for {product_id}{label}"), Some(product_id));
                json!(ctx)
            }
            Request::ProtonList => json!(launcher::list_protons()),
            Request::Playtime { product_id } => json!(settings::playtime(product_id)?),
            Request::RemotePlaytime { product_id } => {
                let online = self.account_for(product_id, None).await?;
                run_local(move || async move {
                    use ubi_core::proto::playtime as pt;
                    let session = online.session().await?;
                    let demux = online.demux().await?;
                    let mut conn = demux.open::<ubi_core::demux::Playtime>().await?;
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
            Request::Sessions => json!(self.sessions.list()),
            Request::Subscribe => Value::Null, // handled by the connection loop
            Request::Shutdown => {
                if !self.running.lock().unwrap().is_empty() {
                    bail!("games are running; stop them first");
                }
                tracing::info!("shutdown requested by {source}");
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = std::fs::remove_file(ubi_api::socket_path());
                    std::process::exit(0);
                });
                Value::Null
            }
        })
    }

    /// A sign-in adds its account (or refreshes it, if already signed in); the first
    /// account becomes the default.
    async fn finish_login(&self, source: &str, outcome: LoginOutcome) -> Result<LoginResult> {
        match outcome {
            LoginOutcome::TwoFactor(c) => {
                *self.pending_2fa.lock().unwrap() = Some(c);
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

    fn start_install(
        self: &Arc<Self>,
        source: &str,
        product_id: u32,
        name: String,
        language: Option<String>,
        online: Arc<Online>,
    ) -> u64 {
        let id = self.new_job(JobKind::Install, product_id, name.clone());
        let dest = Games::load()
            .ok()
            .and_then(|g| g.get(product_id).map(|g| g.install_dir.clone()))
            .unwrap_or_else(|| self.cfg.library.join(launcher::slug(&name)));
        self.log_activity(source, format!("install {name}{}", self.as_account(&online)), Some(product_id));
        let this = self.clone();
        let progress = Arc::new(Progress::default());
        self.installs.lock().unwrap().insert(id, (progress.clone(), false, dest));
        // Progress ticker.
        {
            let this = this.clone();
            let progress = progress.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let done = this.jobs.lock().unwrap().get(&id).is_none_or(|j| j.done);
                    if done {
                        break;
                    }
                    this.update_job(id, |j| {
                        j.downloaded = progress.downloaded.load(Ordering::Relaxed);
                        j.total = progress.total.load(Ordering::Relaxed);
                    });
                }
            });
        }
        tokio::spawn(async move {
            let cfg = this.cfg.clone();
            let lang = language.unwrap_or_else(|| "en-US".into());
            let status_this = this.clone();
            let p = progress.clone();
            let result = run_local(move || async move {
                launcher::install_game(&cfg, &online, product_id, &lang, &p, |s| status_this.update_job(id, |j| j.status = s))
                    .await
            })
            .await;
            let (discard, dest) = this.installs.lock().unwrap().remove(&id).map(|(_, d, p)| (d, Some(p))).unwrap_or_default();
            let cancelled = result.as_ref().err().is_some_and(ubi_core::install::is_cancelled);
            let mut discarded = false;
            if let (true, true, Some(dir)) = (cancelled, discard, dest) {
                // Only when never registered: then the folder holds just this partial install.
                if Games::load().map(|g| g.get(product_id).is_none()).unwrap_or(false) {
                    discarded = std::fs::remove_dir_all(&dir).is_ok();
                }
            }
            *this.library.lock().await = None;
            this.update_job(id, |j| {
                j.done = true;
                j.downloaded = progress.downloaded.load(Ordering::Relaxed);
                match &result {
                    Ok(_) => j.status = "Installed".into(),
                    Err(_) if cancelled => {
                        j.status = if discarded { "Cancelled (files removed)".into() } else { "Cancelled (install again to resume)".into() }
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

    fn start_play(self: &Arc<Self>, source: &str, product_id: u32, online: Arc<Online>) -> Result<u64> {
        let game = Games::load()?.get(product_id).cloned().context("not installed")?;
        if !self.running.lock().unwrap().insert(product_id) {
            bail!("{} is already running", game.name);
        }
        let (s, rev) = settings::current(product_id)?;
        let id = self.new_job(JobKind::Play, product_id, game.name.clone());
        self.log_activity(source, format!("play {}{} (settings r{rev})", game.name, self.as_account(&online)), Some(product_id));
        let this = self.clone();
        tokio::spawn(async move {
            let (cfg, broker) = (this.cfg.clone(), this.broker.clone());
            let sessions = this.sessions.clone();
            let ev_this = this.clone();
            let g = game.clone();
            let started_at: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>> = Arc::default();
            let started = started_at.clone();
            let result = run_local(move || async move {
                launcher::play(&cfg, &online, &g, &s, &broker, &sessions, |e| match e {
                    LaunchEvent::Status(msg) => ev_this.update_job(id, |j| j.status = msg),
                    LaunchEvent::Running => {
                        *started.lock().unwrap() = Some(chrono::Utc::now());
                        ev_this.update_job(id, |j| j.status = "Running".into());
                        ev_this.emit(Event::GameStarted { product_id });
                    }
                    LaunchEvent::Exited(_) => ev_this.update_job(id, |j| j.status = "Game exited".into()),
                })
                .await
            })
            .await;
            this.running.lock().unwrap().remove(&product_id);
            if let Some(start) = *started_at.lock().unwrap() {
                let end = chrono::Utc::now();
                let session = ubi_api::PlaySession {
                    started: start,
                    ended: end,
                    seconds: (end - start).num_seconds().max(0) as u64,
                    settings_revision: rev,
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

async fn serve_conn(daemon: Arc<Daemon>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Reply>();
    let writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            let mut line = serde_json::to_vec(&reply).unwrap_or_default();
            line.push(b'\n');
            if w.write_all(&line).await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let env: Envelope = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                let _ = tx.send(Reply::Err { id: 0, error: format!("bad request: {e}") });
                continue;
            }
        };
        if matches!(env.request, Request::Subscribe) {
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
        let daemon = daemon.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let reply = match daemon.handle(&env.source, env.request).await {
                Ok(result) => Reply::Ok { id: env.id, result },
                Err(e) => Reply::Err { id: env.id, error: format!("{e:#}") },
            };
            let _ = tx.send(reply);
        });
    }
    drop(tx);
    let _ = writer.await;
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "quiverd=info,ubid=info,ubi_core=info".into()),
        )
        .init();
    let path = ubi_api::socket_path();
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
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;

    let cfg = Arc::new(Config::load()?);
    let accounts = Accounts::new();
    // A fixed port lets external bridges (containers, Steam prefixes) find the broker.
    let broker_addr = std::env::var("QUIVER_BROKER")
        .or_else(|_| std::env::var("UBID_BROKER"))
        .unwrap_or_else(|_| "127.0.0.1:47810".into());
    let broker_listener = match tokio::net::TcpListener::bind(&broker_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("broker port {broker_addr} unavailable ({e}); using an ephemeral port");
            tokio::net::TcpListener::bind("127.0.0.1:0").await?
        }
    };
    let broker = broker_listener.local_addr()?.to_string();
    let (events, _) = broadcast::channel(256);
    let sessions = {
        let events = events.clone();
        let list: Arc<Mutex<Option<Arc<PlaySessions>>>> = Arc::default();
        let l = list.clone();
        let s = PlaySessions::new(accounts.clone(), cfg.report_playtime, move || {
            if let Some(s) = &*l.lock().unwrap() {
                let _ = events.send(Event::SessionsChanged { sessions: s.list() });
            }
        });
        *list.lock().unwrap() = Some(s.clone());
        s
    };
    let broker_svc = Broker::new(accounts.clone(), Some(sessions.clone()));
    tokio::spawn(broker_svc.clone().serve(broker_listener));

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
        installs: Mutex::new(BTreeMap::new()),
        library: tokio::sync::Mutex::new(None),
        activity: Mutex::new(activity),
        events,
        sessions,
    });
    tracing::info!("quiverd listening on {} (broker {})", path.display(), daemon.broker);
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve_conn(daemon.clone(), stream));
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

