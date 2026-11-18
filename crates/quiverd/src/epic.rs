//! `Request::Epic`: Epic accounts (design/STORE-EPIC.md §5). The games come through the
//! store-generic requests, from `epic_core::provider::EpicProvider`.
//!
//! Signing in and out are a person's actions: refused from every source but the app and
//! the terminal (STORES.md §7, item 10: "Sign in, sign out … agents: Deny").
//!
//! Installs, updates, repairs and uninstalls of Epic games run here on the shared download
//! queue (`downloads.rs` routes `epic:` items to [`run_queued`]); launches go through
//! `quiver_core::launcher::play_store_game` with the same plan (mods, settings, Options,
//! Tweaks, HUD) a Ubisoft launch gets, and Epic's arguments with a fresh exchange code.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use epic_core::ops::Roots;
use quiver_api::downloads::{InstallPreview, QueueEntry, QueueKind, UpdatePreview};
use quiver_api::epic::EpicRequest;
use quiver_api::{Event, GameRef, JobKind, StoreId};
use quiver_core::install::Progress;
use quiver_core::launcher::{LaunchEvent, SetupRun, StoreLaunch};
use quiver_core::settings;
use serde_json::{Value, json};

use crate::Daemon;
use crate::downloads::{Outcome, Phase, PhaseFn};

fn person_only(source: &str, what: &str) -> Result<()> {
    if !quiver_api::mods::is_human_source(source) {
        bail!("only you can {what}, from the app or a terminal (this request came from '{source}')");
    }
    Ok(())
}

pub(crate) async fn handle(d: &Arc<Daemon>, source: &str, r: EpicRequest) -> Result<Value> {
    Ok(match r {
        EpicRequest::LoginBegin => json!(d.epic.login_start()),
        EpicRequest::LoginComplete { code } => {
            person_only(source, "sign in to Epic")?;
            let a = d.epic.login(&code).await?;
            d.log_activity(source, format!("signed in to Epic as {}", a.display_name), None);
            changed(d);
            // Read the library now, so its catalog is cached before anyone looks.
            let d2 = d.clone();
            tokio::spawn(async move {
                match d2.epic.library(true).await {
                    Ok(lib) => tracing::info!("Epic library: {} game(s)", lib.titles.len()),
                    Err(e) => tracing::info!("Epic library: {e:#}"),
                }
                d2.emit(Event::LibraryChanged);
            });
            json!(a)
        }
        EpicRequest::Logout { account } => {
            person_only(source, "sign out of Epic")?;
            let a = d.epic.logout(account.as_deref()).await?;
            d.log_activity(source, format!("signed out of Epic ({})", a.display_name), None);
            changed(d);
            json!(a)
        }
        EpicRequest::Accounts => json!(d.epic.accounts()?),
        EpicRequest::Hidden => json!(d.epic.hidden_items().await?),
        EpicRequest::Manifest { game } => manifest_summary(d, game).await?,
        EpicRequest::AccountDefault { account } => {
            let a = d.epic.set_default(&account)?;
            d.log_activity(source, format!("default Epic account is now {}", a.display_name), None);
            changed(d);
            json!(a)
        }
    })
}

fn changed(d: &Daemon) {
    d.emit(Event::StoreAccountsChanged { store: StoreId::Epic });
    d.emit(Event::LibraryChanged);
}

// ---------------------------------------------------------------------------------------
// Installs and launches (design/STORE-EPIC.md §9.7)

/// Where Epic games and prefixes go: Quiver's library and prefix folders.
fn roots(d: &Daemon) -> Roots {
    let taken = quiver_core::launcher::Games::load().map(|g| g.games.iter().map(|g| g.prefix.clone()).collect()).unwrap_or_default();
    Roots { library: d.cfg.library.clone(), prefixes: d.cfg.prefixes.clone(), taken_prefixes: taken }
}

fn app(game: GameRef) -> Result<String> {
    if game.store != StoreId::Epic {
        bail!("{game} is not an Epic game");
    }
    Ok(game.id.as_str().to_owned())
}

/// Folders of downloads that haven't finished a first install, for `discard`.
static PENDING: LazyLock<Mutex<HashMap<GameRef, PathBuf>>> = LazyLock::new(Default::default);

/// The game as mods, settings and the launcher see it (an Epic install).
pub(crate) fn mod_game(game: GameRef) -> Result<quiver_core::launcher::Game> {
    let a = app(game)?;
    let installs = epic_core::installs::Installs::new(&quiver_core::launcher::data_dir().join("epic"));
    let i = installs.get(&a).with_context(|| format!("{game} isn't installed (mods need an installed game)"))?;
    epic_core::provider::game_of(&i).context("not a valid game ref")
}

pub(crate) fn is_installed(d: &Daemon, game: GameRef) -> bool {
    app(game).is_ok_and(|a| d.epic.installed_game(&a).is_some())
}

/// `Request::Install` for an Epic game: queue the install, or an update when installed.
pub(crate) async fn install(d: &Arc<Daemon>, source: &str, game: GameRef) -> Result<u64> {
    let a = app(game)?;
    let lib = d.epic.library(false).await?;
    let t = match lib.title(&a) {
        Some(t) => t.clone(),
        None => match lib.hidden(&a) {
            Some(h) => bail!("{game} isn't a game Quiver installs: {}", h.reason),
            None => bail!("{game} is not in your Epic library"),
        },
    };
    if let Some(tp) = &t.third_party {
        match tp.ubisoft_product {
            Some(id) if tp.is_ubisoft() => bail!("{} is a Ubisoft Connect game: install it as ubisoft:{id} once it is on your Ubisoft account", t.title),
            _ => bail!("{} is managed by {}: install it there", t.title, tp.client_name()),
        }
    }
    let kind = if d.epic.installed_game(&a).is_some() { QueueKind::Update } else { QueueKind::Install };
    d.downloads.enqueue(d, source, game, t.title.clone(), kind, None, None)
}

pub(crate) async fn install_preview(d: &Arc<Daemon>, game: GameRef) -> Result<InstallPreview> {
    d.epic.install_preview(&app(game)?, &roots(d)).await
}

pub(crate) async fn update_preview(d: &Arc<Daemon>, game: GameRef) -> Result<UpdatePreview> {
    d.epic.update_preview(&app(game)?).await
}

/// A queued Epic item: install/update, or repair.
pub(crate) async fn run_queued(d: Arc<Daemon>, item: QueueEntry, progress: Arc<Progress>, phase: PhaseFn) -> Result<Outcome> {
    let game = item.game_ref();
    let a = app(game)?;
    let roots = roots(&d);
    let status = {
        let phase = phase.clone();
        move |s: String| phase(Phase::Status(s))
    };
    match item.kind {
        QueueKind::Install | QueueKind::Update => {
            let was = d.epic.installed_game(&a);
            if was.is_none()
                && let Some(t) = d.epic.library(false).await?.title(&a)
            {
                PENDING.lock().unwrap().insert(game, d.epic.folders(t, &roots).0);
            }
            phase(Phase::Downloading);
            let (rec, notes) = d.epic.install(&a, &roots, &progress, &status).await?;
            PENDING.lock().unwrap().remove(&game);
            let verb = match &was {
                None => "Installed",
                Some(w) if w.version == rec.version => "Up to date:",
                Some(_) => "Updated",
            };
            Ok(Outcome { status: format!("{verb} {} ({})", rec.title, rec.version), detail: notes })
        }
        QueueKind::Repair => {
            phase(Phase::Verifying);
            let report = d.epic.repair(&a, &roots, &progress, &status).await?;
            crate::downloads::save_report(&report);
            Ok(Outcome { status: crate::downloads::summary(&report), detail: crate::downloads::report_lines(&report) })
        }
    }
}

/// A cancelled first install's files (only the folder Quiver created for it).
pub(crate) fn discard(d: &Daemon, game: GameRef) -> bool {
    let Ok(a) = app(game) else { return false };
    if d.epic.installed_game(&a).is_some() {
        return false;
    }
    let Some(dir) = PENDING.lock().unwrap().remove(&game) else { return false };
    dir.starts_with(&d.cfg.library) && dir != d.cfg.library && std::fs::remove_dir_all(&dir).is_ok()
}

/// `Request::Verify` for an Epic game: hash now, or queue a repair.
pub(crate) async fn verify(d: &Arc<Daemon>, source: &str, game: GameRef, repair: bool) -> Result<u64> {
    let a = app(game)?;
    let i = d.epic.installed_game(&a).with_context(|| format!("{game} isn't installed"))?;
    if repair {
        return d.downloads.enqueue(d, source, game, i.title, QueueKind::Repair, None, None);
    }
    let (id, progress) = crate::downloads::begin_verify(d, source, game, &i.title)?;
    let this = d.clone();
    tokio::spawn(async move {
        let ticker = crate::downloads::verify_ticker(&this, id, progress.clone());
        let result = this.epic.verify(&a, &progress).await;
        ticker.abort();
        crate::downloads::end_verify(&this, id, &progress, result);
    });
    Ok(id)
}

/// `Request::Uninstall` for an Epic game.
pub(crate) async fn uninstall(d: &Arc<Daemon>, source: &str, game: GameRef, remove_prefix: bool) -> Result<Value> {
    let a = app(game)?;
    if d.running_games.lock().unwrap().contains(&game) {
        bail!("stop the game first");
    }
    d.downloads.ensure_idle(game)?;
    let i = d.epic.uninstall(&a, remove_prefix)?;
    d.log_activity_game(source, format!("uninstalled {}{}", i.title, if remove_prefix { " (and its compatibility folder)" } else { "" }), Some(game));
    *d.library.lock().await = None;
    d.emit(Event::LibraryChanged);
    Ok(json!({"name": i.title, "dir": i.dir, "prefix": i.prefix}))
}

/// `Request::Import` for an Epic game: adopt an existing folder, checked against the
/// current build's manifest; what's missing or different is downloaded. Returns the job.
pub(crate) async fn adopt(d: &Arc<Daemon>, source: &str, game: GameRef, path: PathBuf) -> Result<u64> {
    let a = app(game)?;
    let path = path.canonicalize().with_context(|| format!("{} not found", path.display()))?;
    let name = d.game_name(game).await;
    let id = d.new_game_job(JobKind::Import, game, name.clone());
    d.log_activity_game(source, format!("adopt {} from {}", name, path.display()), Some(game));
    let this = d.clone();
    tokio::spawn(async move {
        let progress = Arc::new(Progress::default());
        let ticker = crate::downloads::verify_ticker(&this, id, progress.clone());
        let status = {
            let this = this.clone();
            move |s: String| this.update_job(id, |j| j.status = s.clone())
        };
        let result = this.epic.adopt(&a, &path, &roots(&this), &progress, &status).await;
        ticker.abort();
        this.update_job(id, |j| {
            j.done = true;
            match &result {
                Ok((rec, report)) => {
                    j.status = format!("Adopted {} ({}): {}", rec.title, rec.version, crate::downloads::summary(report));
                    j.detail = crate::downloads::report_lines(report);
                }
                Err(e) => {
                    j.status = "Failed".into();
                    j.error = Some(format!("{e:#}"));
                }
            }
        });
        if let Ok((_, report)) = &result {
            crate::downloads::save_report(report);
        }
        *this.library.lock().await = None;
        this.emit(Event::LibraryChanged);
    });
    Ok(id)
}

/// `EpicRequest::Manifest`: the current build of a game as Epic serves it, read-only (no
/// download): what an install would write.
async fn manifest_summary(d: &Arc<Daemon>, game: GameRef) -> Result<Value> {
    let a = app(game)?;
    let lib = d.epic.library(false).await?;
    let t = lib.title(&a).cloned().with_context(|| format!("{game} is not a game in your Epic library"))?;
    let account = t.owners.first().cloned().context("no owner")?;
    let b = d.epic.fetch_build(&account, &t.namespace, &t.catalog_item_id, &a).await?;
    let m = &b.manifest;
    let plan = match d.epic.installed_game(&a) {
        Some(i) => epic_core::content::plan(m, d.epic.installed_manifest(&a).ok().as_ref(), &i.dir, i.tags.as_deref()).ok(),
        None => None,
    };
    Ok(json!({
        "game": game,
        "title": t.title,
        "build_version": m.meta.build_version,
        "manifest_version": m.version,
        "manifest_sha1": b.element.hash,
        "encrypted": b.secrets.len(),
        "launch_exe": m.meta.launch_exe,
        "launch_command": m.meta.launch_command,
        "prerequisites": (!m.meta.prereq_path.is_empty()).then(|| json!({"name": m.meta.prereq_name, "path": m.meta.prereq_path, "args": m.meta.prereq_args})),
        "files": m.files.len(),
        "chunks": m.chunks.len(),
        "install_bytes": m.install_size(None),
        "download_bytes": m.download_size(None),
        "install_tags": m.install_tags(),
        "chunk_hosts": b.element.base_urls().iter().filter_map(|u| u.split('/').nth(2).map(str::to_owned)).collect::<Vec<_>>(),
        "first_chunk": m.chunks.first().map(|c| c.path(m.version)),
        "update": plan.map(|p| json!({"write": p.write.len(), "keep": p.keep.len(), "remove": p.remove.len(), "download_bytes": p.download_bytes, "write_bytes": p.write_bytes})),
    }))
}

/// `Request::Play` for an installed Epic game: the same launch plan a Quiver launch of a
/// Ubisoft game gets (mods, settings, Options, Tweaks, the HUD), with Epic's arguments
/// and a fresh exchange code instead of the bridge.
pub(crate) async fn play(d: &Arc<Daemon>, source: &str, game: GameRef, profile: Option<String>) -> Result<u64> {
    let a = app(game)?;
    let installed = d.epic.installed_game(&a).with_context(|| format!("{game} isn't installed: quiver install {game}"))?;
    let g = epic_core::provider::game_of(&installed).context("bad game ref")?;
    if d.downloads.get(game).is_some_and(|e| e.state.is_active()) {
        bail!("{} is downloading; play it once that finishes", g.name);
    }
    let (mut plan, fallback) = crate::mods::launch_plan(&g, profile.as_deref(), source)?;
    if let Some(w) = fallback {
        d.log_activity_game(source, format!("{}: {w}", g.name), Some(game));
    }
    if !plan.api.problems.is_empty() {
        bail!("can't launch {}: {}", g.name, plan.api.problems.join("; "));
    }
    // The licence: a fresh exchange code (and ownership token) for this launch only.
    let run_dir = quiver_core::launcher::run_dir(&g.prefix);
    let ovt_dir = run_dir.clone();
    let material = d.epic.launch_material(&a, |name| quiver_core::launcher::win_path(&ovt_dir.join(name)), false).await?;
    if !d.running_games.lock().unwrap().insert(game) {
        bail!("{} is already running", g.name);
    }
    let (_, rev) = settings::current(game)?;
    let id = d.new_game_job(JobKind::Play, game, g.name.clone());
    let session_id = crate::options::new_session_id();
    plan.session = Some(session_id.clone());
    let mut detail = crate::mods::plan_lines(&plan.api);
    detail.extend(d.options_launch(&g, &mut plan, &session_id));
    let (tweak_lines, tweak_revisions) = d.tweaks_launch(&g, &mut plan, &session_id);
    detail.extend(tweak_lines);
    let perf_profile = plan.api.profile.as_ref().map(|p| p.id.clone());
    detail.extend(d.perf_launch(&session_id, &g, perf_profile.as_deref(), &mut plan.api.settings));
    detail.push(format!("epic: {}", material.shown.join(" ")));
    d.update_job(id, |j| j.detail = detail);
    let with = plan.api.profile.as_ref().map(|p| format!(", profile '{}'", p.name)).unwrap_or_default();
    d.log_activity_game(source, format!("play {} (Epic {}{with}, settings r{rev})", g.name, installed.version), Some(game));

    let mut setup = Vec::new();
    let mut prereq_marker = None;
    if let (Some(p), false) = (&installed.prereq, installed.prereq_done) {
        let marker = format!("epic-prereq-{}", p.ids.join("-").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>());
        prereq_marker = Some(run_dir.join(&marker));
        setup.push(SetupRun {
            name: if p.name.is_empty() { "prerequisites".into() } else { p.name.clone() },
            exe: g.install_dir.join(&p.path),
            args: p.args.split_whitespace().map(str::to_owned).collect(),
            marker,
        });
    }
    let ovt_file = match &material.ownership_token {
        Some((name, token)) => {
            std::fs::create_dir_all(&run_dir)?;
            let path = run_dir.join(name);
            quiver_core::private::write_private(&path, token.expose())?;
            Some(path)
        }
        None => None,
    };
    let store = StoreLaunch { args: material.args, env: Vec::new(), setup };
    let this = d.clone();
    tokio::spawn(async move {
        let started_at: Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>> = Arc::default();
        let started = started_at.clone();
        let exit_code: Arc<Mutex<Option<i32>>> = Arc::default();
        let exit = exit_code.clone();
        let (profile_id, profile_revision) = (plan.api.profile.as_ref().map(|p| p.id.clone()), plan.api.profile.as_ref().map(|_| plan.api.revision));
        let session_mods = plan.api.resolved.clone();
        let options_revision = this.options.launched_revision(&session_id);
        this.options_watch(session_id.clone());
        this.tweaks_watch(session_id.clone());
        let (cfg, ev, sid, g2) = (this.cfg.clone(), this.clone(), session_id.clone(), g.clone());
        let result = crate::run_local(move || async move {
            quiver_core::launcher::play_store_game(&cfg, &g2, &plan, store, |e| match e {
                LaunchEvent::Detail(lines) => ev.update_job(id, |j| j.detail.extend(lines)),
                LaunchEvent::Status(msg) => ev.update_job(id, |j| j.status = msg),
                LaunchEvent::Running => {
                    *started.lock().unwrap() = Some(chrono::Utc::now());
                    ev.update_job(id, |j| j.status = "Running".into());
                    ev.emit(Event::LibraryChanged);
                }
                LaunchEvent::Exited(code) => {
                    *exit.lock().unwrap() = code;
                    ev.update_job(id, |j| j.status = "Game exited".into());
                }
                LaunchEvent::Pids(pids) => {
                    ev.perf.set_pids(&sid, pids.clone());
                    ev.options_pids(&sid, pids)
                }
            })
            .await
        })
        .await;
        if let Some(p) = &ovt_file {
            let _ = std::fs::remove_file(p);
        }
        if prereq_marker.as_ref().is_some_and(|m| m.exists()) {
            let _ = this.epic.prereq_done(&a);
        }
        this.running_games.lock().unwrap().remove(&game);
        let code = *exit_code.lock().unwrap();
        this.options_session_end(&session_id, code).await;
        this.tweaks_session_end(&session_id, 0);
        let perf_summary = this.perf.end(&session_id);
        if let Some(start) = *started_at.lock().unwrap() {
            let end = chrono::Utc::now();
            let session = quiver_api::PlaySession {
                started: start,
                ended: end,
                seconds: (end - start).num_seconds().max(0) as u64,
                settings_revision: rev,
                profile: profile_id,
                profile_revision,
                mods: session_mods,
                session: Some(session_id.clone()),
                options_revision,
                perf: perf_summary,
                tweaks: tweak_revisions,
            };
            if let Err(e) = settings::record_session(game, &session) {
                tracing::warn!("recording play session: {e:#}");
            }
        }
        this.emit(Event::LibraryChanged);
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

/// `Request::Stop` for an Epic game Quiver started: shut its prefix down.
pub(crate) async fn stop(d: &Arc<Daemon>, game: GameRef) -> Result<()> {
    let g = mod_game(game)?;
    if !d.running_games.lock().unwrap().contains(&game) {
        bail!("{} isn't running", g.name);
    }
    let cfg = d.cfg.clone();
    crate::run_local(move || async move { quiver_core::launcher::stop(&cfg, &g).await }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_cannot_sign_in_or_out() {
        assert!(person_only("cli", "x").is_ok() && person_only("gui", "x").is_ok());
        for s in ["agent", "agent:claude", "mcp", "steam", "test"] {
            assert!(person_only(s, "sign in to Epic").unwrap_err().to_string().contains("only you can sign in to Epic"));
        }
    }
}
