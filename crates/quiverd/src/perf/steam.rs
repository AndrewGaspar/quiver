//! Perf sessions for `quiver steam-run` launches (design/SIGHT-HUD.md).
//!
//! quiverd doesn't start a Steam copy itself, so the perf session begins when `steam-run`
//! asks for the launch plan (`SteamLaunchPlan { perf: true }`): its environment (session
//! id, session dir, the Sight layer's variables, the MangoHud remap) goes into the plan's
//! `env`, which `steam-run` applies to Steam's command. It ends with the Steam copy's play
//! session (the bridge's lease, `playsession::Finished`), and its summary is recorded with
//! it. A launch that never reaches the bridge (Steam failed, the user quit at a prompt)
//! doesn't keep its session: it's ended once it has waited [`START_GRACE`] with no play
//! session for the game.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use quiver_api::perf::PerfSummary;
use quiver_api::steamlib::SteamLaunchPlan;

use super::PerfHub;

/// How long a Steam launch's perf session waits for the game's play session to start.
pub const START_GRACE: Duration = Duration::from_secs(10 * 60);

/// Perf sessions begun for Steam launches, by product.
#[derive(Default)]
pub struct SteamSessions(Mutex<HashMap<u32, Pending>>);

struct Pending {
    session: String,
    since: Instant,
    /// The game's play session has been seen (so only its end ends this).
    started: bool,
}

impl SteamSessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u32, Pending>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PerfHub {
    /// Begin the perf session of a Steam launch and add its environment to `plan`. A session
    /// still waiting from an earlier launch of the same game is ended first. Errors leave
    /// the plan as it was.
    pub fn begin_steam(&self, plan: &mut SteamLaunchPlan) -> Result<()> {
        let product_id = plan.product_id;
        if let Some(old) = self.steam.lock().remove(&product_id) {
            tracing::info!(session = %old.session, product_id, "ending a Steam launch's perf session for a new launch");
            self.end(&old.session);
        }
        if self.product_running(product_id) {
            anyhow::bail!("the game already has a perf session");
        }
        let session = quiver_core::modsdk::new_session_id();
        let b = self.begin(&session, product_id, None, None, &plan.env, &plan.wrappers, false)?;
        if b.plan.hud.as_ref().is_some_and(|h| h.enabled) {
            plan.perf_notes.extend(b.plan.notes.into_iter().map(|n| format!("hud: {n}")));
        }
        plan.env.extend(b.plan.env);
        plan.perf_session = Some(session.clone());
        self.steam.lock().insert(product_id, Pending { session, since: Instant::now(), started: false });
        Ok(())
    }

    /// The Steam copy's play session ended: end its perf session, with the summary.
    pub fn end_steam(&self, product_id: u32) -> Option<PerfSummary> {
        let p = self.steam.lock().remove(&product_id)?;
        self.end(&p.session)
    }

    /// Housekeeping, every so often: `playing` says whether the game has a play session
    /// now. Sessions whose game started are kept until [`Self::end_steam`]; the others end
    /// after [`START_GRACE`]. Returns the sessions ended.
    pub fn sweep_steam(&self, playing: impl Fn(u32) -> bool, now: Instant) -> Vec<String> {
        let mut gone = Vec::new();
        self.steam.lock().retain(|product, p| {
            p.started |= playing(*product);
            let keep = p.started || now.duration_since(p.since) < START_GRACE;
            if !keep {
                gone.push(p.session.clone());
            }
            keep
        });
        for s in &gone {
            tracing::info!(session = %s, "a Steam launch never started its game; ending its perf session");
            self.end(s);
        }
        gone
    }

    /// The perf session of a Steam launch of the game, if one is open.
    #[cfg(test)]
    pub fn steam_session(&self, product_id: u32) -> Option<String> {
        self.steam.lock().get(&product_id).map(|p| p.session.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_sessions_begin_end_and_expire() {
        let shm = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let hub = PerfHub::new(data.path().to_path_buf(), shm.path().to_path_buf());
        let mut plan = SteamLaunchPlan { product_id: 7, ..Default::default() };
        hub.begin_steam(&mut plan).unwrap();
        let first = plan.perf_session.clone().unwrap();
        assert!(hub.product_running(7));
        assert_eq!(hub.steam_session(7).as_deref(), Some(first.as_str()));
        // The HUD is off by default: no session dir, so nothing added to the environment.
        assert!(plan.env.is_empty(), "{:?}", plan.env);

        // A second launch replaces a session still waiting.
        let mut again = SteamLaunchPlan { product_id: 7, ..Default::default() };
        hub.begin_steam(&mut again).unwrap();
        let second = again.perf_session.unwrap();
        assert_ne!(first, second);
        assert_eq!(hub.sessions().len(), 1);

        // Not started within the grace: ended. Started: kept until the play session ends.
        let later = Instant::now() + START_GRACE + Duration::from_secs(1);
        assert!(hub.sweep_steam(|_| true, Instant::now()).is_empty());
        assert!(hub.sweep_steam(|_| false, later).is_empty(), "it started, so only its end ends it");
        hub.end_steam(7);
        assert!(!hub.product_running(7));

        let mut lost = SteamLaunchPlan { product_id: 8, ..Default::default() };
        hub.begin_steam(&mut lost).unwrap();
        assert_eq!(hub.sweep_steam(|_| false, later), [lost.perf_session.unwrap()]);
        assert!(hub.sessions().is_empty());
        assert!(hub.end_steam(8).is_none());
    }

    #[test]
    fn steam_session_with_the_hud_gets_its_environment() {
        let shm = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let hub = PerfHub::new(data.path().to_path_buf(), shm.path().to_path_buf());
        let on = quiver_api::perf::HudScope { enabled: Some(true), ..Default::default() };
        hub.hud.apply(&quiver_api::perf::HudScopeRef::game(9), &on, &[], "test", None).unwrap();
        let mut plan = SteamLaunchPlan { product_id: 9, wrappers: vec!["mangohud".into()], ..Default::default() };
        hub.begin_steam(&mut plan).unwrap();
        let sid = plan.perf_session.clone().unwrap();
        assert_eq!(plan.env.get("QUIVER_SESSION"), Some(&sid));
        assert!(plan.env.contains_key("QUIVER_PERF_DIR"), "{:?}", plan.env);
        let dir = std::path::PathBuf::from(&plan.env["QUIVER_PERF_DIR"]);
        assert!(dir.is_dir());
        hub.end_steam(9);
        assert!(!dir.exists(), "the session dir goes with the session");
    }
}
