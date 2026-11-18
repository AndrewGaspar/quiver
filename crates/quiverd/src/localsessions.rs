//! Play sessions of Steam games started through `quiver steam-run` that aren't Ubisoft copies
//! (design/STORE-STEAM.md §9). Ubisoft games' sessions are `PlaySessions` (leases that report
//! to Ubisoft); these are local only: they show in `Request::Sessions` (with `game`), keep
//! the launch's perf session alive, block a daemon restart like any running game, and go into
//! the game's play history (`games/steam-<appid>/playtime.jsonl`) when they end.
//!
//! `steam-run` registers its own pid (`Request::SteamSession`) and then execs Steam's
//! command, so the pid lives exactly as long as Steam's launch: Steam's reaper (or the
//! command Steam put first) runs as that process. quiverd holds a pidfd for it (no polling,
//! no pid reuse) and ends the session when it exits. Nothing of Steam's process tree changes:
//! Steam's Stop, its overlay and Steam Input work as without the hook.
//!
//! A quiverd that restarts while the game runs loses the session (the restart waits for
//! running games, so this needs a crash); the game itself is unaffected.

use std::collections::BTreeMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use quiver_api::tweaks::TweakRevisions;
use quiver_api::{ActiveSession, GameRef};

/// The holder a local Steam session shows (`ActiveSession.holders`).
pub const HOLDER: &str = "steam";

#[derive(Debug, Clone)]
pub struct Local {
    pub game: GameRef,
    pub pid: u32,
    pub started: DateTime<Utc>,
    pub perf_session: Option<String>,
    pub settings_revision: u32,
    pub tweaks: Option<TweakRevisions>,
    /// The mods launch quiverd prepared (`SteamLaunchPlan { mods: true }`): its profile, mod
    /// versions and options, and the options session that ends with this one.
    pub mods: Option<(String, crate::steammods::Prepared)>,
}

impl Local {
    pub fn active(&self) -> ActiveSession {
        let prepared = self.mods.as_ref().map(|(_, p)| p);
        ActiveSession {
            product_id: 0,
            started: self.started,
            holders: vec![HOLDER.to_owned()],
            reported: false,
            steam_app_id: self.game.as_steam(),
            session: self.perf_session.clone().or_else(|| self.mods.as_ref().map(|(id, _)| id.clone())),
            pids: vec![self.pid],
            game: Some(self.game),
            profile: prepared.and_then(|p| p.profile.clone()),
            options_revision: prepared.and_then(|p| p.options_revision),
            ..Default::default()
        }
    }
}

#[derive(Default)]
pub struct LocalSessions {
    by_id: Mutex<BTreeMap<u64, Local>>,
    next: AtomicU64,
}

impl LocalSessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Local>> {
        self.by_id.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn insert(&self, l: Local) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, l);
        id
    }

    pub fn remove(&self, id: u64) -> Option<Local> {
        self.lock().remove(&id)
    }

    pub fn list(&self) -> Vec<ActiveSession> {
        self.lock().values().map(Local::active).collect()
    }

    /// The session of `pid`, if one is open (a second registration of the same process).
    pub fn of_pid(&self, pid: u32) -> Option<u64> {
        self.lock().iter().find(|(_, l)| l.pid == pid).map(|(id, _)| *id)
    }
}

/// A pidfd for `pid` (Linux 5.3+): readable once the process exits.
pub fn pidfd_open(pid: u32) -> Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).context("bad pid")?;
    if pid <= 1 {
        bail!("bad pid {pid}");
    }
    // SAFETY: a plain syscall; on success it returns a new file descriptor we own.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        bail!("process {pid}: {e}");
    }
    // SAFETY: `fd` is a fresh descriptor, owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Wait until the process behind `fd` exits.
pub async fn exited(fd: OwnedFd) -> Result<()> {
    // SAFETY: the AsyncFd owns `fd` (an OwnedFd) for its whole life, so the descriptor stays
    // open and the same.
    let afd = unsafe { tokio::io::unix::AsyncFd::register_with_interest(fd, tokio::io::Interest::READABLE) }.map_err(std::io::Error::from)?;
    let _ = afd.readable().await?;
    Ok(())
}

/// The play session to record for a local session that ended at `ended`.
pub fn record(l: &Local, ended: DateTime<Utc>, perf: Option<quiver_api::perf::PerfSummary>) -> quiver_api::PlaySession {
    quiver_api::PlaySession {
        started: l.started,
        ended,
        seconds: (ended - l.started).num_seconds().max(0) as u64,
        settings_revision: l.settings_revision,
        perf,
        tweaks: l.tweaks,
        profile: l.mods.as_ref().and_then(|(_, p)| p.profile.clone()),
        profile_revision: l.mods.as_ref().and_then(|(_, p)| p.profile_revision),
        mods: l.mods.as_ref().map(|(_, p)| p.mods.clone()).unwrap_or_default(),
        session: l.mods.as_ref().map(|(id, _)| id.clone()).or_else(|| l.perf_session.clone()),
        options_revision: l.mods.as_ref().and_then(|(_, p)| p.options_revision),
        ..Default::default()
    }
}

impl crate::Daemon {
    /// Every game being played: Ubisoft sessions (`PlaySessions`), then local Steam ones.
    pub(crate) fn all_sessions(&self) -> Vec<ActiveSession> {
        let mut v = self.sessions.list();
        v.extend(self.local.list());
        v
    }

    /// `Request::SteamSession`: hold a local play session for Steam app `app_id` while `pid`
    /// lives. A second registration of the same process returns its session.
    pub(crate) async fn steam_session(
        self: &std::sync::Arc<Self>,
        source: &str,
        app_id: u32,
        pid: u32,
        perf_session: Option<String>,
        settings_revision: u32,
        mods_launch: Option<String>,
    ) -> Result<ActiveSession> {
        if app_id == 0 {
            bail!("no Steam app id");
        }
        let game = GameRef::steam(app_id);
        if let Some(id) = self.local.of_pid(pid) {
            return self.local.lock().get(&id).map(Local::active).context("session vanished");
        }
        let fd = pidfd_open(pid).context("watching the launch's process")?;
        let tweaks = quiver_core::tweaks::Scopes::load(&quiver_core::launcher::data_dir(), Some(&game), None).ok().map(|s| s.revisions());
        let mods = mods_launch.and_then(|id| crate::steammods::claim(&id, game).map(|p| (id, p)));
        let with = match mods.as_ref().map(|(_, p)| p) {
            Some(p) => match &p.profile {
                Some(id) => format!(" with profile {id}{}", if p.mods.is_empty() { String::new() } else { format!(" ({})", p.mods.iter().map(|m| m.describe()).collect::<Vec<_>>().join(" < ")) }),
                None if !p.mods.is_empty() => format!(" with fixes ({})", p.mods.iter().map(|m| m.describe()).collect::<Vec<_>>().join(" < ")),
                None => " (vanilla)".to_owned(),
            },
            None => String::new(),
        };
        let l = Local { game, pid, started: Utc::now(), perf_session, settings_revision, tweaks, mods };
        let active = l.active();
        let id = self.local.insert(l);
        let name = self.game_name(game).await;
        self.log_activity_game(source, format!("playing {name} from Steam{with}, through quiver steam-run"), Some(game));
        self.emit(quiver_api::Event::SessionsChanged { sessions: self.all_sessions() });
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(e) = exited(fd).await {
                tracing::warn!(%game, pid, "watching a Steam launch: {e:#}; ending its session");
            }
            this.end_local(id, &name).await;
        });
        Ok(active)
    }

    /// A local session's process exited: end its perf session and record it.
    async fn end_local(self: &std::sync::Arc<Self>, id: u64, name: &str) {
        let Some(l) = self.local.remove(id) else { return };
        // Its options session (outside edits imported, crash correlation), as for a Quiver
        // launch. Steam doesn't say how the game exited.
        if let Some((launch, _)) = &l.mods {
            self.options_session_end(launch, None).await;
        }
        let ended = Utc::now();
        // Only the perf session this launch began (a newer launch may have its own by now).
        let perf = l.perf_session.as_deref().and_then(|s| self.perf.end_steam_session(&l.game, s));
        let session = record(&l, ended, perf);
        tracing::info!(game = %l.game, seconds = session.seconds, "a Steam launch through quiver steam-run ended");
        if let Err(e) = quiver_core::settings::record_session(l.game, &session) {
            tracing::warn!("recording play session: {e:#}");
        }
        let mins = session.seconds / 60;
        self.log_activity_game("steam", format!("played {name} for {mins} min (Steam, through quiver steam-run)"), Some(l.game));
        self.emit(quiver_api::Event::SessionsChanged { sessions: self.all_sessions() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_list_with_their_game() {
        let s = LocalSessions::default();
        assert!(s.list().is_empty());
        let control = GameRef::steam(870780);
        let id = s.insert(Local { game: control, pid: 4242, started: Utc::now(), perf_session: Some("s-x".into()), settings_revision: 3, tweaks: None, mods: None });
        let a = &s.list()[0];
        assert_eq!((a.product_id, a.game, a.game_ref(), a.steam_app_id), (0, Some(control), control, Some(870780)));
        assert_eq!((a.holders.as_slice(), a.session.as_deref(), a.pids.as_slice()), (&["steam".to_owned()][..], Some("s-x"), &[4242][..]));
        // On the wire: `game`, and the product 0 older clients skip.
        let v = serde_json::to_value(a).unwrap();
        assert_eq!((v["game"].as_str(), v["product_id"].as_u64()), (Some("steam:870780"), Some(0)));
        assert_eq!(s.of_pid(4242), Some(id));
        let l = s.remove(id).unwrap();
        let ended = l.started + chrono::TimeDelta::minutes(90);
        let p = record(&l, ended, None);
        assert_eq!((p.seconds, p.settings_revision), (5400, 3));
        assert!(s.list().is_empty() && s.remove(id).is_none());
    }

    #[tokio::test]
    async fn a_pidfd_says_when_the_process_exits() {
        let mut child = std::process::Command::new("sleep").arg("0.2").spawn().unwrap();
        let fd = pidfd_open(child.id()).unwrap();
        let t = std::time::Instant::now();
        let waiter = tokio::spawn(exited(fd));
        // Reaped by its parent (this test), as Steam's launch is reaped by Steam.
        tokio::task::spawn_blocking(move || child.wait()).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await.unwrap().unwrap().unwrap();
        assert!(t.elapsed() >= std::time::Duration::from_millis(150));
        assert!(pidfd_open(1).is_err());
        assert!(pidfd_open(u32::MAX).is_err());
    }
}
