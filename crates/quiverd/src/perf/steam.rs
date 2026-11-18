//! Perf sessions for `quiver steam-run` launches (design/SIGHT-HUD.md).
//!
//! quiverd doesn't start a Steam copy itself, so the perf session begins when `steam-run`
//! asks for the launch plan (`SteamLaunchPlan { perf: true }`): its environment (session
//! id, session dir, the Sight layer's variables, the MangoHud remap) goes into the plan's
//! `env`, which `steam-run` applies to Steam's command. It ends with the game's play
//! session (a Steam copy's: the bridge's lease, `playsession::Finished`; any other Steam
//! game's: `steam-run`'s local session, `crate::localsessions`), and its summary is recorded
//! with it. A launch that never starts its play session (Steam failed, the user quit at a
//! prompt) doesn't keep its perf session: it's ended once it has waited [`START_GRACE`] with
//! no play session for the game.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use quiver_api::GameRef;
use quiver_api::perf::PerfSummary;
use quiver_api::steamlib::SteamLaunchPlan;

use super::PerfHub;

/// How long a Steam launch's perf session waits for the game's play session to start.
pub const START_GRACE: Duration = Duration::from_secs(10 * 60);

/// Perf sessions begun for Steam launches, by game.
#[derive(Default)]
pub struct SteamSessions(Mutex<HashMap<GameRef, Pending>>);

struct Pending {
    session: String,
    since: Instant,
    /// The game's play session has been seen (so only its end ends this).
    started: bool,
}

impl SteamSessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GameRef, Pending>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl PerfHub {
    /// Begin the perf session of a Steam launch and add its environment to `plan`. A session
    /// still waiting from an earlier launch of the same game is ended first. Errors leave
    /// the plan as it was.
    ///
    /// `steam_env`: what Steam set for the launch (kept by `steam-run`), seen under the plan's
    /// own environment for the layer order and MangoHud.
    pub fn begin_steam(&self, plan: &mut SteamLaunchPlan, steam_env: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let game = plan.game_ref();
        if let Some(old) = self.steam.lock().remove(&game) {
            tracing::info!(session = %old.session, %game, "ending a Steam launch's perf session for a new launch");
            self.end(&old.session);
        }
        if self.running(&game) {
            anyhow::bail!("the game already has a perf session");
        }
        // One id for the launch: its mods' (options) session when it has one.
        let session = plan.mods.as_ref().map(|m| m.launch.clone()).unwrap_or_else(quiver_core::modsdk::new_session_id);
        let mut env = steam_env.clone();
        env.extend(plan.env.clone());
        // The install folder, for the anti-cheat rule: Steam's own variable (`steam-run`
        // passes it on), else the app's manifest (not in unit tests: that's the host's Steam).
        let install = steam_env
            .get(quiver_core::steamplan::STEAM_INSTALL_VAR)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| game.as_steam().filter(|_| !cfg!(test)).and_then(|_| crate::mods::installed(game).ok()).map(|g| g.install_dir));
        let b = self.begin(&session, game, None, None, install.as_deref(), &env, &plan.wrappers, false)?;
        plan.perf_notes.extend(b.plan.notes.into_iter().map(|n| if n.starts_with("hud:") { n } else { format!("hud: {n}") }));
        plan.env.extend(b.plan.env);
        plan.perf_session = Some(session.clone());
        self.steam.lock().insert(game, Pending { session, since: Instant::now(), started: false });
        Ok(())
    }

    /// The game's play session ended: end its perf session, with the summary.
    pub fn end_steam(&self, game: &GameRef) -> Option<PerfSummary> {
        let p = self.steam.lock().remove(game)?;
        self.end(&p.session)
    }

    /// The game's play session ended, and it was the launch that began `session`: end that
    /// perf session (not a newer launch's).
    pub fn end_steam_session(&self, game: &GameRef, session: &str) -> Option<PerfSummary> {
        let mut st = self.steam.lock();
        if st.get(game).is_some_and(|p| p.session == session) {
            st.remove(game);
        }
        drop(st);
        self.end(session)
    }

    /// Housekeeping, every so often: `playing` says whether the game has a play session
    /// now. Sessions whose game started are kept until [`Self::end_steam`]; the others end
    /// after [`START_GRACE`]. Returns the sessions ended.
    pub fn sweep_steam(&self, playing: impl Fn(&GameRef) -> bool, now: Instant) -> Vec<String> {
        let mut gone = Vec::new();
        self.steam.lock().retain(|game, p| {
            p.started |= playing(game);
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
    pub fn steam_session(&self, game: &GameRef) -> Option<String> {
        self.steam.lock().get(game).map(|p| p.session.clone())
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
        hub.begin_steam(&mut plan, &Default::default()).unwrap();
        let first = plan.perf_session.clone().unwrap();
        assert!(hub.product_running(7));
        assert_eq!(hub.steam_session(&GameRef::ubisoft(7)).as_deref(), Some(first.as_str()));
        // The layer is in by default (the HUD hidden): the session's environment.
        assert_eq!(plan.env.get("QUIVER_SESSION"), Some(&first));
        assert_eq!(plan.env.get("QUIVER_SIGHT").map(String::as_str), Some("1"));

        // A second launch replaces a session still waiting.
        let mut again = SteamLaunchPlan { product_id: 7, ..Default::default() };
        hub.begin_steam(&mut again, &Default::default()).unwrap();
        let second = again.perf_session.unwrap();
        assert_ne!(first, second);
        assert_eq!(hub.sessions().len(), 1);

        // Not started within the grace: ended. Started: kept until the play session ends.
        let later = Instant::now() + START_GRACE + Duration::from_secs(1);
        assert!(hub.sweep_steam(|_| true, Instant::now()).is_empty());
        assert!(hub.sweep_steam(|_| false, later).is_empty(), "it started, so only its end ends it");
        hub.end_steam(&GameRef::ubisoft(7));
        assert!(!hub.product_running(7));

        let mut lost = SteamLaunchPlan { product_id: 8, ..Default::default() };
        hub.begin_steam(&mut lost, &Default::default()).unwrap();
        assert_eq!(hub.sweep_steam(|_| false, later), [lost.perf_session.unwrap()]);
        assert!(hub.sessions().is_empty());
        assert!(hub.end_steam(&GameRef::ubisoft(8)).is_none());
    }

    #[test]
    fn steam_session_with_the_hud_gets_its_environment() {
        let shm = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let hub = PerfHub::new(data.path().to_path_buf(), shm.path().to_path_buf());
        let on = quiver_api::perf::HudScope { enabled: Some(true), ..Default::default() };
        hub.hud.apply(&quiver_api::perf::HudScopeRef::game(9), &on, &[], "test", None).unwrap();
        let mut plan = SteamLaunchPlan { product_id: 9, wrappers: vec!["mangohud".into()], ..Default::default() };
        hub.begin_steam(&mut plan, &Default::default()).unwrap();
        let sid = plan.perf_session.clone().unwrap();
        assert_eq!(plan.env.get("QUIVER_SESSION"), Some(&sid));
        assert!(plan.env.contains_key("QUIVER_PERF_DIR"), "{:?}", plan.env);
        let dir = std::path::PathBuf::from(&plan.env["QUIVER_PERF_DIR"]);
        assert!(dir.is_dir());
        hub.end_steam(&GameRef::ubisoft(9));
        assert!(!dir.exists(), "the session dir goes with the session");
    }

    #[test]
    fn any_steam_game_gets_a_perf_session_and_its_hud() {
        let shm = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let hub = PerfHub::new(data.path().to_path_buf(), shm.path().to_path_buf());
        let control = GameRef::steam(870780);
        let on = quiver_api::perf::HudScope { enabled: Some(true), ..Default::default() };
        hub.hud.apply(&quiver_api::perf::HudScopeRef::of(control), &on, &[], "test", None).unwrap();
        let mut plan = SteamLaunchPlan::for_game(control);
        plan.env.insert("ENABLE_VKBASALT".into(), "1".into());
        hub.begin_steam(&mut plan, &Default::default()).unwrap();
        let sid = plan.perf_session.clone().unwrap();
        assert_eq!(plan.env.get("QUIVER_SESSION"), Some(&sid));
        assert_eq!(plan.env.get("QUIVER_SIGHT").map(String::as_str), Some("1"));
        // The same layer order as a Quiver launch: vkBasalt above Sight.
        assert_eq!(plan.env["VK_INSTANCE_LAYERS"], "VK_LAYER_VKBASALT_post_processing:VK_LAYER_QUIVER_sight");
        assert!(hub.running(&control) && !hub.product_running(0));
        let info = hub.sessions().pop().unwrap();
        assert_eq!((info.product_id, info.game), (0, Some(control)));
        assert_eq!(hub.status(None, Some(control)).unwrap().game_ref(), control);
        // Another Steam game with the layer turned off: a session, no environment.
        let off = quiver_api::perf::HudScope { enabled: Some(false), ..Default::default() };
        hub.hud.apply(&quiver_api::perf::HudScopeRef::of(GameRef::steam(620)), &off, &[], "test", None).unwrap();
        let mut other = SteamLaunchPlan::for_game(GameRef::steam(620));
        hub.begin_steam(&mut other, &Default::default()).unwrap();
        assert!(other.env.is_empty());
        assert!(other.perf_notes.iter().any(|n| n.contains("no Sight layer")), "{:?}", other.perf_notes);
        // A game with anti-cheat in Steam's install folder: no layer either.
        let install = tempfile::tempdir().unwrap();
        std::fs::write(install.path().join("start_protected_game.exe"), b"").unwrap();
        let eac_env: std::collections::BTreeMap<String, String> = [(quiver_core::steamplan::STEAM_INSTALL_VAR.to_owned(), install.path().display().to_string())].into();
        let mut eac = SteamLaunchPlan::for_game(GameRef::steam(1172470));
        hub.begin_steam(&mut eac, &eac_env).unwrap();
        assert!(!eac.env.contains_key("QUIVER_SIGHT"), "{:?}", eac.env);
        assert!(eac.perf_notes.iter().any(|n| n.contains("anti-cheat: EasyAntiCheat (start_protected_game.exe)")), "{:?}", eac.perf_notes);
        // Layers the person turned on in Steam's launch options stay above Sight; their own
        // list stays first. Steam's variables aren't copied into the plan.
        let cp = GameRef::steam(1091500);
        hub.hud.apply(&quiver_api::perf::HudScopeRef::of(cp), &on, &[], "test", None).unwrap();
        let steam_env: std::collections::BTreeMap<String, String> =
            [("ENABLE_LSFG", "1"), ("VK_INSTANCE_LAYERS", "VK_LAYER_mine")].into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        let mut p = SteamLaunchPlan::for_game(cp);
        hub.begin_steam(&mut p, &steam_env).unwrap();
        assert_eq!(p.env["VK_INSTANCE_LAYERS"], "VK_LAYER_mine:VK_LAYER_LS_frame_generation:VK_LAYER_QUIVER_sight");
        assert!(!p.env.contains_key("ENABLE_LSFG"));
        // A newer launch's session isn't ended by an older launch's end.
        assert!(hub.end_steam_session(&GameRef::steam(620), "s-not-this-one").is_none());
        assert!(hub.steam_session(&GameRef::steam(620)).is_some());
        hub.end_steam_session(&control, &sid);
        assert!(!hub.running(&control) && hub.steam_session(&control).is_none());
    }
}
