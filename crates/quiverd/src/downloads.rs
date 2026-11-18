//! Downloads: the install queue, pre-flight previews, updates, verify/repair and download
//! settings.
//!
//! Installs, updates and repairs go through one queue: `max_active` run at a time (one by
//! default), the rest wait their turn. Pausing stops a download at a slice boundary
//! (through the installer's cancel flag) and resuming continues from what is on disk. The
//! queue is saved to `<data>/queue.json` on every change; after a restart, items that
//! were running start again, queued ones wait their turn and paused ones stay paused.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use quiver_api::downloads::{DownloadSettings, QueueEntry, QueueKind, QueueMove, QueueState, VerifyReport};
use quiver_api::{Event, GameRef, Job, JobKind, LibraryEntry, Request};
use quiver_core::builds::{self, Latest, RepairPhase};
use quiver_core::install::{Progress, Throttle, human_bytes, is_cancelled};
use quiver_core::launcher::{self, Config, Games};
use quiver_core::settings;

use crate::{Daemon, run_local};

pub(crate) type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What a running item reports.
pub(crate) enum Phase {
    Status(String),
    Verifying,
    Downloading,
}

pub(crate) type PhaseFn = Arc<dyn Fn(Phase) + Send + Sync>;

/// How a finished item ends: its job's last status line and extra lines.
pub(crate) struct Outcome {
    pub status: String,
    pub detail: Vec<String>,
}

/// What the queue needs from the daemon (a stand-in in tests).
pub(crate) trait Host: Send + Sync + 'static {
    fn new_job(&self, kind: JobKind, game: GameRef, name: String) -> u64;
    fn update_job(&self, id: u64, f: &mut dyn FnMut(&mut Job));
    fn log(&self, source: &str, action: String, game: Option<GameRef>);
    /// The game is being played: its install waits until it exits.
    fn playing(&self, game: GameRef) -> bool;
    /// The library if the daemon has it cached (never fetches).
    fn library_cache(&self) -> Option<Vec<LibraryEntry>>;
    /// An item finished (whatever the outcome): install state may have changed.
    fn changed(self: Arc<Self>);
    /// Delete what a removed download left, if the game was never installed.
    fn discard(&self, game: GameRef, name: &str) -> bool;
    /// Do the work of an item.
    fn run(self: Arc<Self>, item: QueueEntry, progress: Arc<Progress>, phase: PhaseFn) -> BoxFut<Result<Outcome>>;
    /// Whether new items may start (not while quiverd restarts onto a new build).
    fn accepting(&self) -> bool {
        true
    }
}

enum Stop {
    Pause,
    Remove { discard: bool },
}

struct Item {
    entry: QueueEntry,
    /// While running.
    progress: Option<Arc<Progress>>,
    stop: Option<Stop>,
    /// Latest status line from the worker.
    status: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    #[serde(default)]
    items: Vec<QueueEntry>,
}

pub(crate) struct Downloads {
    items: Mutex<Vec<Item>>,
    path: PathBuf,
    settings: Mutex<DownloadSettings>,
    throttle: Arc<Throttle>,
    /// Products whose queued update the user removed (not auto-queued again this run).
    dismissed: Mutex<HashSet<GameRef>>,
    /// Verify-only jobs running: product -> (job id, progress).
    verifying: Mutex<Vec<(GameRef, u64, Arc<Progress>)>>,
}

fn status_line(e: &QueueEntry, waiting: bool) -> Option<String> {
    Some(match e.state {
        QueueState::Queued if waiting => format!("Queued · #{} · waiting for the game to exit", e.position),
        QueueState::Queued => format!("Queued · #{}", e.position),
        QueueState::Paused => "Paused".into(),
        QueueState::Verifying => "Verifying…".into(),
        QueueState::Failed => "Failed".into(),
        QueueState::Downloading | QueueState::Done => return None,
    })
}

impl Downloads {
    /// Load the saved queue. Items that were running are queued again (and start first,
    /// in their place); paused and failed ones stay as they were.
    pub(crate) fn load(path: PathBuf, settings: DownloadSettings) -> Arc<Self> {
        let saved: Saved = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).map_err(|e| tracing::warn!("ignoring corrupt {}: {e}", path.display())).ok())
            .unwrap_or_default();
        let items = saved
            .items
            .into_iter()
            .filter(|e| e.state != QueueState::Done)
            .map(|mut entry| {
                if entry.state.is_active() {
                    entry.state = QueueState::Queued;
                }
                entry.job_id = None;
                entry.speed = 0;
                entry.retries = 0;
                Item { entry, progress: None, stop: None, status: None }
            })
            .collect();
        let throttle = Arc::new(Throttle::new(settings.bandwidth_limit.unwrap_or(0)));
        let d = Arc::new(Self {
            items: Mutex::new(items),
            path,
            settings: Mutex::new(settings),
            throttle,
            dismissed: Mutex::default(),
            verifying: Mutex::default(),
        });
        d.renumber();
        d
    }

    /// Create jobs for the loaded items and start what can start.
    pub(crate) fn start<H: Host>(self: &Arc<Self>, host: &Arc<H>) {
        {
            let mut items = self.items.lock().unwrap();
            for it in items.iter_mut() {
                let kind = JobKind::Install;
                let id = host.new_job(kind, it.entry.game_ref(), it.entry.name.clone());
                it.entry.job_id = Some(id);
                let (status, error) = (status_line(&it.entry, false), it.entry.error.clone());
                let failed = it.entry.state == QueueState::Failed;
                host.update_job(id, &mut |j| {
                    if let Some(s) = &status {
                        j.status = s.clone();
                    }
                    if failed {
                        j.done = true;
                        j.error = error.clone();
                    }
                });
            }
        }
        if !self.items.lock().unwrap().is_empty() {
            tracing::info!("download queue restored: {}", self.describe());
        }
        self.pump(host);
    }

    fn describe(&self) -> String {
        let items = self.items.lock().unwrap();
        items.iter().map(|i| format!("{} ({:?})", i.entry.name, i.entry.state)).collect::<Vec<_>>().join(", ")
    }

    pub(crate) fn settings(&self) -> DownloadSettings {
        self.settings.lock().unwrap().clone()
    }

    pub(crate) fn set_settings<H: Host>(self: &Arc<Self>, host: &Arc<H>, s: DownloadSettings) {
        self.throttle.set_rate(s.bandwidth_limit.unwrap_or(0));
        *self.settings.lock().unwrap() = s;
        self.pump(host);
    }

    fn renumber(&self) {
        for (i, it) in self.items.lock().unwrap().iter_mut().enumerate() {
            it.entry.position = i as u32 + 1;
        }
    }

    pub(crate) fn list(&self) -> Vec<QueueEntry> {
        self.items.lock().unwrap().iter().map(|i| i.entry.clone()).collect()
    }

    pub(crate) fn get(&self, game: GameRef) -> Option<QueueEntry> {
        self.items.lock().unwrap().iter().find(|i| i.entry.game_ref() == game).map(|i| i.entry.clone())
    }

    fn save(&self) {
        let saved = Saved { items: self.list() };
        let tmp = self.path.with_extension("json.tmp");
        let r = (|| -> Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?)?;
            std::fs::rename(&tmp, &self.path)?;
            Ok(())
        })();
        if let Err(e) = r {
            tracing::warn!("saving {}: {e:#}", self.path.display());
        }
    }

    /// Save and refresh the jobs' queue lines. Every item's job gets an update (an
    /// `Event::Job`), which is how front ends learn the queue changed.
    fn changed<H: Host>(&self, host: &Arc<H>) {
        self.renumber();
        self.save();
        let lines: Vec<(Option<u64>, Option<String>)> = self
            .items
            .lock()
            .unwrap()
            .iter()
            .map(|i| {
                let waiting = i.entry.state == QueueState::Queued && host.playing(i.entry.game_ref());
                (i.entry.job_id, status_line(&i.entry, waiting).or_else(|| i.status.clone()))
            })
            .collect();
        for (id, line) in lines {
            if let (Some(id), Some(line)) = (id, line) {
                host.update_job(id, &mut |j| {
                    if !j.done {
                        j.status = line.clone();
                    }
                });
            }
        }
    }

    /// Add an item, or return the job of the one already queued for this game (a paused or
    /// failed one is resumed).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue<H: Host>(
        self: &Arc<Self>,
        host: &Arc<H>,
        source: &str,
        game: GameRef,
        name: String,
        kind: QueueKind,
        language: Option<String>,
        account: Option<String>,
    ) -> Result<u64> {
        if self.verifying.lock().unwrap().iter().any(|(p, ..)| *p == game) {
            bail!("{name} is being verified; wait for it or cancel it first");
        }
        if source != "auto-update" {
            self.dismissed.lock().unwrap().remove(&game);
        }
        if let Some(e) = self.get(game) {
            if e.kind != kind && !matches!((e.kind, kind), (QueueKind::Install | QueueKind::Update, QueueKind::Install | QueueKind::Update)) {
                bail!("{name} is already in the queue ({:?}); remove it first", e.kind);
            }
            if matches!(e.state, QueueState::Paused | QueueState::Failed) {
                return self.resume(host, source, game);
            }
            return e.job_id.context("queued item has no job");
        }
        let id = host.new_job(JobKind::Install, game, name.clone());
        let entry = QueueEntry {
            product_id: game.as_ubisoft().unwrap_or(0),
            game: (!game.is_ubisoft()).then_some(game),
            name: name.clone(),
            kind,
            state: QueueState::Queued,
            position: 0,
            job_id: Some(id),
            language,
            account,
            source: source.to_owned(),
            added: chrono::Utc::now(),
            error: None,
            done_bytes: 0,
            total_bytes: 0,
            speed: 0,
            retries: 0,
        };
        self.items.lock().unwrap().push(Item { entry, progress: None, stop: None, status: None });
        let what = match kind {
            QueueKind::Install => "install",
            QueueKind::Update => "update",
            QueueKind::Repair => "repair",
        };
        host.log(source, format!("queued {what} of {name}"), Some(game));
        self.changed(host);
        self.pump(host);
        Ok(id)
    }

    pub(crate) fn move_item<H: Host>(&self, host: &Arc<H>, game: GameRef, to: QueueMove) -> Result<()> {
        {
            let mut items = self.items.lock().unwrap();
            let i = items.iter().position(|i| i.entry.game_ref() == game).context("not in the queue")?;
            match to {
                QueueMove::Up if i > 0 => items.swap(i, i - 1),
                QueueMove::Down if i + 1 < items.len() => items.swap(i, i + 1),
                QueueMove::Top => {
                    let it = items.remove(i);
                    items.insert(0, it);
                }
                _ => {}
            }
        }
        self.changed(host);
        Ok(())
    }

    pub(crate) fn pause<H: Host>(&self, host: &Arc<H>, source: &str, game: GameRef) -> Result<()> {
        let name = {
            let mut items = self.items.lock().unwrap();
            let it = items.iter_mut().find(|i| i.entry.game_ref() == game).context("not in the queue")?;
            match it.entry.state {
                QueueState::Queued => it.entry.state = QueueState::Paused,
                s if s.is_active() => {
                    it.stop = Some(Stop::Pause);
                    it.status = Some("Pausing…".into());
                    if let Some(p) = &it.progress {
                        p.cancel.store(true, Ordering::Relaxed);
                    }
                }
                QueueState::Paused => return Ok(()),
                s => bail!("{} can't be paused ({s:?})", it.entry.name),
            }
            it.entry.name.clone()
        };
        host.log(source, format!("paused {name}"), Some(game));
        self.changed(host);
        Ok(())
    }

    pub(crate) fn resume<H: Host>(self: &Arc<Self>, host: &Arc<H>, source: &str, game: GameRef) -> Result<u64> {
        let (name, job) = {
            let mut items = self.items.lock().unwrap();
            let it = items.iter_mut().find(|i| i.entry.game_ref() == game).context("not in the queue")?;
            match it.entry.state {
                QueueState::Paused | QueueState::Failed => {
                    if it.entry.state == QueueState::Failed {
                        // Its job ended with the failure; a retry gets a new one.
                        it.entry.job_id = Some(host.new_job(JobKind::Install, game, it.entry.name.clone()));
                        it.entry.error = None;
                    }
                    it.entry.state = QueueState::Queued;
                }
                _ if it.stop.is_some() => {
                    // Pausing, not yet stopped: keep going instead.
                    it.stop = None;
                    it.status = None;
                    if let Some(p) = &it.progress {
                        p.cancel.store(false, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
            (it.entry.name.clone(), it.entry.job_id)
        };
        host.log(source, format!("resumed {name}"), Some(game));
        self.changed(host);
        self.pump(host);
        job.context("queued item has no job")
    }

    /// Take a game out of the queue; a running item stops first (at a slice boundary).
    pub(crate) fn remove<H: Host>(self: &Arc<Self>, host: &Arc<H>, source: &str, game: GameRef, discard: bool) -> Result<()> {
        let removed = {
            let mut items = self.items.lock().unwrap();
            let i = items.iter().position(|i| i.entry.game_ref() == game).context("not in the queue")?;
            if items[i].progress.is_some() {
                let it = &mut items[i];
                it.stop = Some(Stop::Remove { discard });
                it.status = Some("Cancelling…".into());
                if let Some(p) = &it.progress {
                    p.cancel.store(true, Ordering::Relaxed);
                }
                None
            } else {
                Some(items.remove(i))
            }
        };
        if let Some(it) = &removed {
            let discarded = discard && host.discard(game, &it.entry.name);
            if let Some(id) = it.entry.job_id {
                host.update_job(id, &mut |j| {
                    if !j.done {
                        j.done = true;
                        j.status = if discarded { "Cancelled (files removed)".into() } else { "Removed from queue".into() };
                    }
                });
            }
        }
        self.dismissed.lock().unwrap().insert(game);
        host.log(source, format!("removed {} from the queue{}", host_name(self, game, &removed), if discard { ", discarding files" } else { "" }), Some(game));
        self.changed(host);
        self.pump(host);
        Ok(())
    }

    /// Refuse while a game has a queue item running or a verify going.
    /// What is running, in words ("installing X", "verifying Y"): these block restarting
    /// onto a new build. Queued, paused and failed items don't (the queue is saved).
    pub(crate) fn running(&self, mut name: impl FnMut(GameRef) -> String) -> Vec<String> {
        let mut out: Vec<String> = self
            .items
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.progress.is_some())
            .map(|i| {
                let verb = match i.entry.kind {
                    QueueKind::Install => "installing",
                    QueueKind::Update => "updating",
                    QueueKind::Repair => "repairing",
                };
                format!("{verb} {}", i.entry.name)
            })
            .collect();
        let verifying: Vec<GameRef> = self.verifying.lock().unwrap().iter().map(|(p, ..)| *p).collect();
        out.extend(verifying.into_iter().map(|p| format!("verifying {}", name(p))));
        out
    }

    pub(crate) fn ensure_idle(&self, game: GameRef) -> Result<()> {
        if self.items.lock().unwrap().iter().any(|i| i.entry.game_ref() == game && i.progress.is_some()) {
            bail!("a download for this game is running; pause or remove it first (quiver queue)");
        }
        if self.verifying.lock().unwrap().iter().any(|(p, ..)| *p == game) {
            bail!("this game is being verified");
        }
        Ok(())
    }

    /// Start queued items, top first, while fewer than `max_active` run.
    pub(crate) fn pump<H: Host>(self: &Arc<Self>, host: &Arc<H>) {
        if !host.accepting() {
            return;
        }
        let max = self.settings.lock().unwrap().max_active.max(1);
        let mut started = Vec::new();
        {
            let mut items = self.items.lock().unwrap();
            let mut active = items.iter().filter(|i| i.progress.is_some()).count();
            for it in items.iter_mut() {
                if active >= max {
                    break;
                }
                if it.entry.state != QueueState::Queued || it.progress.is_some() || host.playing(it.entry.game_ref()) {
                    continue;
                }
                let progress = Arc::new(Progress { throttle: Some(self.throttle.clone()), ..Progress::default() });
                it.progress = Some(progress.clone());
                it.entry.state = QueueState::Downloading;
                it.entry.done_bytes = 0;
                it.entry.total_bytes = 0;
                it.status = Some("Starting…".into());
                active += 1;
                started.push((it.entry.clone(), progress));
            }
        }
        if started.is_empty() {
            return;
        }
        self.changed(host);
        for (entry, progress) in started {
            self.spawn(host, entry, progress);
        }
    }

    fn spawn<H: Host>(self: &Arc<Self>, host: &Arc<H>, entry: QueueEntry, progress: Arc<Progress>) {
        let game = entry.game_ref();
        let job = entry.job_id;
        let phase: PhaseFn = {
            let (this, host) = (self.clone(), host.clone());
            Arc::new(move |p: Phase| {
                let state = match p {
                    Phase::Status(s) => {
                        if let Some(it) = this.items.lock().unwrap().iter_mut().find(|i| i.entry.game_ref() == game) {
                            it.status = Some(s.clone());
                        }
                        if let Some(id) = job {
                            host.update_job(id, &mut |j| j.status = s.clone());
                        }
                        return;
                    }
                    Phase::Verifying => QueueState::Verifying,
                    Phase::Downloading => QueueState::Downloading,
                };
                if let Some(it) = this.items.lock().unwrap().iter_mut().find(|i| i.entry.game_ref() == game) {
                    it.entry.state = state;
                    if state == QueueState::Downloading {
                        it.status = Some("Downloading…".into());
                    }
                }
                this.changed(&host);
            })
        };
        // Progress ticker.
        {
            let (this, host, progress) = (self.clone(), host.clone(), progress.clone());
            tokio::spawn(async move {
                let mut last = (Instant::now(), 0u64);
                let mut speed = 0f64;
                let mut was_retrying = false;
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let (done, total, line) = {
                        let mut items = this.items.lock().unwrap();
                        let Some(it) = items.iter_mut().find(|i| i.progress.as_ref().is_some_and(|p| Arc::ptr_eq(p, &progress))) else { break };
                        let (done, total) = if it.entry.state == QueueState::Verifying {
                            (progress.hashed.load(Ordering::Relaxed), progress.hash_total.load(Ordering::Relaxed))
                        } else {
                            (progress.downloaded.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed))
                        };
                        let now = Instant::now();
                        let dt = now.duration_since(last.0).as_secs_f64();
                        let rate = done.saturating_sub(last.1) as f64 / dt.max(0.001);
                        speed = if speed == 0.0 { rate } else { 0.7 * speed + 0.3 * rate };
                        last = (now, done);
                        it.entry.done_bytes = done;
                        it.entry.total_bytes = total;
                        it.entry.speed = speed as u64;
                        let retries = progress.retries.load(Ordering::Relaxed);
                        it.entry.retries = retries;
                        // While slices wait to be tried again, say so (then back to the
                        // worker's own line).
                        let retrying = progress.retrying.load(Ordering::Relaxed) > 0 && it.stop.is_none();
                        let line = if retrying {
                            Some(format!("Retrying… ({retries} {})", if retries == 1 { "retry" } else { "retries" }))
                        } else if was_retrying {
                            it.status.clone()
                        } else {
                            None
                        };
                        was_retrying = retrying;
                        (done, total, line)
                    };
                    if let Some(id) = job {
                        host.update_job(id, &mut |j| {
                            j.downloaded = done;
                            j.total = total;
                            if let Some(l) = &line {
                                j.status = l.clone();
                            }
                        });
                    }
                }
            });
        }
        let (this, host) = (self.clone(), host.clone());
        tokio::spawn(async move {
            let result = host.clone().run(entry, progress.clone(), phase).await;
            this.finish(&host, game, &progress, result);
        });
    }

    fn finish<H: Host>(self: &Arc<Self>, host: &Arc<H>, game: GameRef, progress: &Arc<Progress>, result: Result<Outcome>) {
        let cancelled = result.as_ref().err().is_some_and(is_cancelled);
        let (job, done_job, removed, discard, name) = {
            let mut items = self.items.lock().unwrap();
            let Some(i) = items.iter().position(|i| i.progress.as_ref().is_some_and(|p| Arc::ptr_eq(p, progress))) else { return };
            let it = &mut items[i];
            it.progress = None;
            it.status = None;
            let stop = it.stop.take();
            let job = it.entry.job_id;
            let name = it.entry.name.clone();
            match (&result, stop) {
                (Err(_), Some(Stop::Pause)) if cancelled => {
                    it.entry.state = QueueState::Paused;
                    (job, None, false, false, name)
                }
                (Err(_), Some(Stop::Remove { discard })) if cancelled => {
                    items.remove(i);
                    (job, Some(("Removed from queue (install again to resume)".to_owned(), None, Vec::new())), true, discard, name)
                }
                (Err(_), None) if cancelled => {
                    // Resumed while it was stopping: start it again.
                    it.entry.state = QueueState::Queued;
                    (job, None, false, false, name)
                }
                (Ok(o), _) => {
                    items.remove(i);
                    (job, Some((o.status.clone(), None, o.detail.clone())), true, false, name)
                }
                (Err(e), _) => {
                    it.entry.state = QueueState::Failed;
                    it.entry.error = Some(format!("{e:#}"));
                    (job, Some(("Failed".to_owned(), Some(format!("{e:#}")), Vec::new())), false, false, name)
                }
            }
        };
        let discarded = discard && host.discard(game, &name);
        if let (Some(id), Some((status, error, detail))) = (job, done_job) {
            let status = if discarded { "Cancelled (files removed)".to_owned() } else { status };
            let downloaded = progress.downloaded.load(Ordering::Relaxed);
            host.update_job(id, &mut |j| {
                j.done = true;
                j.downloaded = downloaded;
                j.status = status.clone();
                j.error = error.clone();
                j.detail = detail.clone();
            });
        }
        if removed {
            tracing::info!(game = %game, "download finished");
        }
        host.clone().changed();
        self.changed(host);
        self.pump(host);
    }

    /// Queue updates for installed games the cached library shows one for (auto-update).
    fn auto_update<H: Host>(self: &Arc<Self>, host: &Arc<H>) {
        if !self.settings.lock().unwrap().auto_update {
            return;
        }
        let Some(lib) = host.library_cache() else { return };
        let updates = lib.into_iter().filter(|e| e.installed.is_some() && e.update_available).filter_map(|e| Some((GameRef::ubisoft(e.ubisoft_id()?), e)));
        for (game, e) in updates {
            if self.get(game).is_some() || self.dismissed.lock().unwrap().contains(&game) || host.playing(game) {
                continue;
            }
            let language = e.installed.as_ref().and_then(|g| g.language.clone());
            if let Err(err) = self.enqueue(host, "auto-update", game, e.name.clone(), QueueKind::Update, language, None) {
                tracing::warn!(game = %game, "auto-update: {err:#}");
            }
        }
    }

    /// Background upkeep: start items whose game stopped, auto-update.
    pub(crate) fn spawn_upkeep<H: Host>(self: &Arc<Self>, host: &Arc<H>) {
        let (this, host) = (self.clone(), host.clone());
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                this.pump(&host);
                n += 1;
                if n % 6 == 0 {
                    this.auto_update(&host);
                }
            }
        });
    }
}

fn host_name(d: &Downloads, game: GameRef, removed: &Option<Item>) -> String {
    removed
        .as_ref()
        .map(|i| i.entry.name.clone())
        .or_else(|| d.get(game).map(|e| e.name))
        .unwrap_or_else(|| game.to_string())
}

// ---------------------------------------------------------------------------------------
// The daemon's side

impl Host for Daemon {
    fn new_job(&self, kind: JobKind, game: GameRef, name: String) -> u64 {
        Daemon::new_game_job(self, kind, game, name)
    }

    fn update_job(&self, id: u64, f: &mut dyn FnMut(&mut Job)) {
        Daemon::update_job(self, id, |j| f(j))
    }

    fn log(&self, source: &str, action: String, game: Option<GameRef>) {
        self.log_activity_game(source, action, game)
    }

    fn playing(&self, game: GameRef) -> bool {
        match game.as_ubisoft() {
            Some(product_id) => self.running.lock().unwrap().contains(&product_id) || self.sessions.list().iter().any(|s| s.product_id == product_id),
            None => self.running_games.lock().unwrap().contains(&game),
        }
    }

    fn library_cache(&self) -> Option<Vec<LibraryEntry>> {
        self.library.try_lock().ok().and_then(|l| l.clone())
    }

    fn accepting(&self) -> bool {
        !self.lifecycle.closed()
    }

    fn changed(self: Arc<Self>) {
        tokio::spawn(async move {
            *self.library.lock().await = None;
            self.emit(Event::LibraryChanged);
        });
    }

    fn discard(&self, game: GameRef, name: &str) -> bool {
        let Some(product_id) = game.as_ubisoft() else { return crate::epic::discard(self, game) };
        // Only when never registered: then the folder holds just this partial install.
        if Games::load().map(|g| g.get(product_id).is_some()).unwrap_or(true) {
            return false;
        }
        let dir = builds::dest_for(&self.cfg, product_id, name);
        dir.starts_with(&self.cfg.library) && std::fs::remove_dir_all(&dir).is_ok()
    }

    fn run(self: Arc<Self>, item: QueueEntry, progress: Arc<Progress>, phase: PhaseFn) -> BoxFut<Result<Outcome>> {
        Box::pin(async move {
            if !item.game_ref().is_ubisoft() {
                return crate::epic::run_queued(self, item, progress, phase).await;
            }
            let product_id = item.product_id;
            let online = self.account_for(product_id, item.account.as_deref()).await?;
            let cfg = self.cfg.clone();
            let lang = item.language.clone().unwrap_or_else(|| language_for(product_id));
            match item.kind {
                QueueKind::Install | QueueKind::Update => {
                    let ph = phase.clone();
                    let was_installed = Games::load()?.get(product_id).is_some();
                    let game = run_local(move || async move {
                        launcher::install_game(&cfg, &online, product_id, &lang, &progress, |s| ph(Phase::Status(s))).await
                    })
                    .await?;
                    let status = if was_installed { "Updated" } else { "Installed" };
                    Ok(Outcome { status: format!("{status} {}", game.name), detail: Vec::new() })
                }
                QueueKind::Repair => {
                    let game = Games::load()?.get(product_id).cloned().context("not installed")?;
                    let ph = phase.clone();
                    let report = run_local(move || async move {
                        builds::repair_game(&online, &game, &lang, progress, |p| match p {
                            RepairPhase::Verifying => ph(Phase::Verifying),
                            RepairPhase::Downloading(n) => {
                                ph(Phase::Downloading);
                                ph(Phase::Status(format!("Re-downloading {}…", human_bytes(n))));
                            }
                        })
                        .await
                    })
                    .await?;
                    save_report(&report);
                    Ok(Outcome { status: summary(&report), detail: report_lines(&report) })
                }
            }
        })
    }
}

/// The language to install a game in: its setting, else what it was installed with.
fn language_for(product_id: u32) -> String {
    settings::current(product_id)
        .ok()
        .and_then(|(s, _)| s.language)
        .or_else(|| Games::load().ok().and_then(|g| g.get(product_id).and_then(|g| g.language.clone())))
        .unwrap_or_else(|| "en-US".into())
}

pub(crate) fn report_path(game: GameRef) -> PathBuf {
    quiver_core::paths::game_dir(&game).join("verify.json")
}

pub(crate) fn save_report(r: &VerifyReport) {
    let path = report_path(r.game.unwrap_or(GameRef::ubisoft(r.product_id)));
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Err(e) = serde_json::to_vec_pretty(r).map_err(anyhow::Error::from).and_then(|b| Ok(std::fs::write(&path, b)?)) {
        tracing::warn!("saving {}: {e:#}", path.display());
    }
}

pub(crate) fn summary(r: &VerifyReport) -> String {
    let bad = r.missing.len() + r.corrupt.len();
    match (bad, r.repaired_files) {
        (0, _) => format!("All {} files OK", r.files),
        (_, 0) => format!("{} missing, {} corrupt of {} files", r.missing.len(), r.corrupt.len(), r.files),
        (_, n) => format!("Repaired {n} files ({} downloaded)", human_bytes(r.repaired_bytes)),
    }
}

pub(crate) fn report_lines(r: &VerifyReport) -> Vec<String> {
    let mut out: Vec<String> = r.missing.iter().map(|p| format!("missing: {p}")).collect();
    out.extend(r.corrupt.iter().map(|c| {
        let size = if c.size != c.expected_size { format!(", {} bytes, expected {}", c.size, c.expected_size) } else { String::new() };
        format!("corrupt: {} ({} bad slice(s){size})", c.path, c.bad_slices)
    }));
    out
}

async fn product_name(daemon: &Arc<Daemon>, product_id: u32) -> String {
    daemon
        .library(false)
        .await
        .ok()
        .and_then(|l| l.into_iter().find(|e| e.ubisoft_id() == Some(product_id)).map(|e| e.name))
        .unwrap_or_else(|| daemon.product_name(product_id))
}

/// `Request::Install`: queue an install, or an update if the game is installed.
pub(crate) async fn install(daemon: &Arc<Daemon>, source: &str, product_id: u32, language: Option<String>) -> Result<u64> {
    let name = product_name(daemon, product_id).await;
    // Resolve the account now, so a missing sign-in fails here rather than in the queue.
    daemon.account_for(product_id, None).await?;
    let installed = Games::load()?.get(product_id).is_some();
    let kind = if installed { QueueKind::Update } else { QueueKind::Install };
    let language = language.or_else(|| Some(language_for(product_id)));
    daemon.downloads.enqueue(daemon, source, GameRef::ubisoft(product_id), name, kind, language, None)
}

/// `Request::Cancel` for a queue (`Install`) or verify job: removes the queue item.
pub(crate) fn cancel(daemon: &Arc<Daemon>, source: &str, job: &Job, discard: bool) -> Result<()> {
    if job.kind == JobKind::Verify {
        let v = daemon.downloads.verifying.lock().unwrap();
        let (_, _, p) = v.iter().find(|(_, id, _)| *id == job.id).context("verify not running")?;
        p.cancel.store(true, Ordering::Relaxed);
        return Ok(());
    }
    match daemon.downloads.get(job.game) {
        Some(e) if e.job_id == Some(job.id) => daemon.downloads.remove(daemon, source, job.game, discard),
        _ => bail!("job {} is not in the download queue", job.id),
    }
}

/// Start a verify-only job for `game`: refuses while it downloads or is being verified.
/// Returns the job id and the progress the verify reports to.
pub(crate) fn begin_verify(daemon: &Arc<Daemon>, source: &str, game: GameRef, name: &str) -> Result<(u64, Arc<Progress>)> {
    if daemon.downloads.get(game).is_some_and(|e| e.state.is_active()) {
        bail!("{name} is downloading; verify it once that finishes");
    }
    if daemon.downloads.verifying.lock().unwrap().iter().any(|(p, ..)| *p == game) {
        bail!("{name} is already being verified");
    }
    let id = daemon.new_game_job(JobKind::Verify, game, name.to_owned());
    let progress = Arc::new(Progress::default());
    daemon.downloads.verifying.lock().unwrap().push((game, id, progress.clone()));
    daemon.log_activity_game(source, format!("verify {name}"), Some(game));
    daemon.update_job(id, |j| j.status = "Verifying…".into());
    Ok((id, progress))
}

/// End a verify-only job started by [`begin_verify`].
pub(crate) fn end_verify(daemon: &Arc<Daemon>, id: u64, progress: &Progress, result: Result<VerifyReport>) {
    daemon.downloads.verifying.lock().unwrap().retain(|(_, j, _)| *j != id);
    daemon.update_job(id, |j| {
        j.done = true;
        j.downloaded = progress.hashed.load(Ordering::Relaxed);
        match &result {
            Ok(r) => {
                j.status = summary(r);
                j.detail = report_lines(r);
            }
            Err(e) if is_cancelled(e) => j.status = "Cancelled".into(),
            Err(e) => {
                j.status = "Failed".into();
                j.error = Some(format!("{e:#}"));
            }
        }
    });
    if let Ok(r) = &result {
        save_report(r);
    }
}

/// A ticker copying a verify's hashing progress to its job every half second.
pub(crate) fn verify_ticker(daemon: &Arc<Daemon>, id: u64, progress: Arc<Progress>) -> tokio::task::JoinHandle<()> {
    let this = daemon.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            this.update_job(id, |j| {
                j.downloaded = progress.hashed.load(Ordering::Relaxed);
                j.total = progress.hash_total.load(Ordering::Relaxed);
            });
        }
    })
}

fn verify_now(daemon: &Arc<Daemon>, source: &str, product_id: u32) -> Result<u64> {
    let game = Games::load()?.get(product_id).cloned().context("not installed")?;
    if daemon.downloads.get(GameRef::ubisoft(product_id)).is_some_and(|e| e.state.is_active()) {
        bail!("{} is downloading; verify it once that finishes", game.name);
    }
    if daemon.downloads.verifying.lock().unwrap().iter().any(|(p, ..)| *p == GameRef::ubisoft(product_id)) {
        bail!("{} is already being verified", game.name);
    }
    let id = daemon.new_job(JobKind::Verify, product_id, game.name.clone());
    let progress = Arc::new(Progress::default());
    daemon.downloads.verifying.lock().unwrap().push((GameRef::ubisoft(product_id), id, progress.clone()));
    daemon.log_activity(source, format!("verify {}", game.name), Some(product_id));
    daemon.update_job(id, |j| j.status = "Verifying…".into());
    let this = daemon.clone();
    tokio::spawn(async move {
        let ticker = {
            let (this, progress) = (this.clone(), progress.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    this.update_job(id, |j| {
                        j.downloaded = progress.hashed.load(Ordering::Relaxed);
                        j.total = progress.hash_total.load(Ordering::Relaxed);
                    });
                }
            })
        };
        let lang = language_for(product_id);
        let p = progress.clone();
        let result = tokio::task::spawn_blocking(move || builds::verify_game(&game, &lang, &p)).await;
        ticker.abort();
        this.downloads.verifying.lock().unwrap().retain(|(_, j, _)| *j != id);
        let result = result.map_err(anyhow::Error::from).and_then(|r| r);
        this.update_job(id, |j| {
            j.done = true;
            j.downloaded = progress.hashed.load(Ordering::Relaxed);
            match &result {
                Ok((_, v)) => {
                    j.status = summary(&v.report);
                    j.detail = report_lines(&v.report);
                }
                Err(e) if is_cancelled(e) => j.status = "Cancelled".into(),
                Err(e) => {
                    j.status = "Failed".into();
                    j.error = Some(format!("{e:#}"));
                }
            }
        });
        if let Ok((_, v)) = &result {
            save_report(&v.report);
        }
    });
    Ok(id)
}

/// The latest build, through the game's account.
async fn latest(daemon: &Arc<Daemon>, product_id: u32) -> Result<(Latest, Arc<quiver_core::online::Online>)> {
    let online = daemon.account_for(product_id, None).await?;
    let o = online.clone();
    let latest = run_local(move || async move { Latest::fetch(&o, product_id).await }).await?;
    Ok((latest, online))
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    let d = &daemon.downloads;
    Ok(match req {
        Request::Queue => json!(d.list()),
        Request::QueueMove { game, to } => {
            d.move_item(daemon, game.0, to)?;
            json!(d.list())
        }
        Request::QueuePause { game } => {
            d.pause(daemon, source, game.0)?;
            Value::Null
        }
        Request::QueueResume { game } => json!(d.resume(daemon, source, game.0)?),
        Request::QueueRemove { game } => {
            d.remove(daemon, source, game.0, false)?;
            Value::Null
        }
        Request::InstallPreview { game, .. } if !game.is_ubisoft() => json!(crate::epic::install_preview(daemon, game.0).await?),
        Request::UpdatePreview { game } if !game.is_ubisoft() => json!(crate::epic::update_preview(daemon, game.0).await?),
        Request::Update { game } if !game.is_ubisoft() => json!(crate::epic::install(daemon, source, game.0).await?),
        Request::Verify { game, repair } if !game.is_ubisoft() => json!(crate::epic::verify(daemon, source, game.0, repair).await?),
        Request::VerifyResult { game } if !game.is_ubisoft() => {
            let r: Option<VerifyReport> = std::fs::read_to_string(report_path(game.0)).ok().and_then(|t| serde_json::from_str(&t).ok());
            json!(r)
        }
        Request::InstallPreview { game, language } => {
            let product_id = ubisoft(game.0)?;
            let lang = language.unwrap_or_else(|| language_for(product_id));
            let (latest, _) = latest(daemon, product_id).await?;
            let cfg = daemon.cfg.clone();
            json!(tokio::task::spawn_blocking(move || builds::install_preview(&cfg, product_id, &latest, &lang)).await??)
        }
        Request::UpdatePreview { game } => {
            let product_id = ubisoft(game.0)?;
            let game = Games::load()?.get(product_id).cloned().context("not installed")?;
            let lang = language_for(product_id);
            let (latest, _) = latest(daemon, product_id).await?;
            json!(tokio::task::spawn_blocking(move || builds::update_preview(&game, &latest, &lang)).await??)
        }
        Request::Update { game } => {
            let product_id = ubisoft(game.0)?;
            Games::load()?.get(product_id).context("not installed")?;
            daemon.account_for(product_id, None).await?;
            let name = product_name(daemon, product_id).await;
            json!(d.enqueue(daemon, source, game.0, name, QueueKind::Update, Some(language_for(product_id)), None)?)
        }
        Request::Verify { game, repair: false } => json!(verify_now(daemon, source, ubisoft(game.0)?)?),
        Request::Verify { game, repair: true } => {
            let product_id = ubisoft(game.0)?;
            let g = Games::load()?.get(product_id).cloned().context("not installed")?;
            daemon.account_for(product_id, None).await?;
            json!(d.enqueue(daemon, source, game.0, g.name, QueueKind::Repair, Some(language_for(product_id)), None)?)
        }
        Request::VerifyResult { game } => {
            let r: Option<VerifyReport> = std::fs::read_to_string(report_path(game.0)).ok().and_then(|t| serde_json::from_str(&t).ok());
            json!(r)
        }
        Request::DownloadSettings { max_active, bandwidth_limit, auto_update } => {
            let mut s = d.settings();
            let before = s.clone();
            if let Some(n) = max_active {
                if n == 0 {
                    bail!("max_active must be at least 1");
                }
                s.max_active = n;
            }
            if let Some(b) = bandwidth_limit {
                s.bandwidth_limit = (b > 0).then_some(b);
            }
            if let Some(a) = auto_update {
                s.auto_update = a;
            }
            if s != before {
                let mut cfg = Config::load()?;
                cfg.downloads = s.clone();
                cfg.save()?;
                daemon.log_activity(source, format!("download settings: {}", serde_json::to_string(&s)?), None);
                d.set_settings(daemon, s.clone());
            }
            json!(s)
        }
        other => bail!("not a downloads request: {other:?}"),
    })
}

/// A Ubisoft product id, or why a request for another store's game can't be served here.
pub(crate) fn ubisoft(game: GameRef) -> Result<u32> {
    game.as_ubisoft().with_context(|| format!("{game}: not a Ubisoft game"))
}

/// Load the queue for the daemon.
pub(crate) fn load(cfg: &Config) -> Arc<Downloads> {
    Downloads::load(launcher::data_dir().join("queue.json"), cfg.downloads.clone())
}

/// Start the queue once the daemon is up.
pub(crate) fn start(daemon: &Arc<Daemon>) {
    daemon.downloads.start(daemon);
    daemon.downloads.spawn_upkeep(daemon);
}

#[cfg(test)]
mod tests;
