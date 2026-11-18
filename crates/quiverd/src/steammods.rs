//! Mods for Steam games launched through `quiver steam-run` (design/STEAM-COPIES.md §4,
//! design/STORE-STEAM.md §10).
//!
//! A Steam game is a [`Game`] to the mod store: its install folder is Steam's
//! (`<library>/steamapps/common/<game>`), its prefix Steam's compatibility folder
//! (`<library>/steamapps/compatdata/<appid>/pfx`) for a Proton game, none for a native one.
//! Its mods, profiles, options and histories live under `games/steam-<appid>/` and
//! `mods/steam-<appid>/`, keyed by [`GameRef::dir_name`]; Ubisoft games' folders don't move.
//!
//! **A launch** (`SteamLaunchPlan { mods: true }`): quiverd picks the profile (`steam-run
//! --profile`, else the game's `steam_profile` setting, else vanilla), builds the same plan a
//! Quiver launch gets (`mods::launch_plan`), renders its options, applies its config patches
//! to Steam's prefix (originals recorded in `patches.json`, as for any game), and returns the
//! overlays to mount. `steam-run` mounts them in a user + mount namespace of its own and runs
//! Steam's command inside it (`quiver_core::steamrun::run_in_namespace`), so Steam's reaper,
//! pressure-vessel, Proton and the game all see the mods; Steam itself and every other
//! process see the real install.
//!
//! The prepared launch is remembered here by its id until `SteamSession { mods_launch }`
//! claims it, so the play session records the profile, mod versions and options revision it
//! ran, and its options session ends with it.
//!
//! **Saves.** Steam Cloud syncs the game's real save folder, so every profile of a Steam game
//! uses it (`savestores::REAL_SAVES`): forking saves per profile isn't built for Steam games.
//! Nothing here ever touches Steam's own files (`localconfig.vdf`, manifests, the client).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use quiver_api::mods::ResolvedMod;
use quiver_api::steamlib::{SteamLaunchPlan, SteamMods, SteamOverlay};
use quiver_api::{Game, GameRef};
use quiver_core::exports::Paths;
use quiver_core::exports::steam::Steam;
use quiver_core::exports::steamgames::{self, AppManifest};

use crate::Daemon;

/// A Steam app as mods see it (read-only: Steam's app manifest).
pub(crate) fn mod_game(app_id: u32) -> Result<Game> {
    let steam = Steam::find(&Paths::from_env().home).context("Steam not found (looked in ~/.steam/steam, ~/.local/share/Steam and the Flatpak's data)")?;
    let app = steamgames::find_app(&steam, app_id).with_context(|| format!("steam:{app_id} isn't installed in any Steam library (mods need an installed game)"))?;
    Ok(game_of(&app))
}

/// [`mod_game`] for a manifest: the prefix is Steam's compatibility folder when Steam has
/// made one for the app (a Proton game; the prefix itself may not exist before its first
/// run), else none (a native game).
pub(crate) fn game_of(app: &AppManifest) -> Game {
    let steamapps = app.install_dir.parent().and_then(|p| p.parent()).map(PathBuf::from).unwrap_or_else(|| app.library.join("steamapps"));
    let compat = steamapps.join("compatdata").join(app.app_id.to_string());
    Game {
        product_id: 0,
        name: app.name.clone(),
        install_dir: app.install_dir.clone(),
        prefix: if compat.is_dir() { compat.join("pfx") } else { PathBuf::new() },
        exe: String::new(),
        args: None,
        language: None,
        game: Some(GameRef::steam(app.app_id)),
    }
}

/// A Steam app's name, from its manifest.
pub(crate) fn app_name(app_id: u32) -> Option<String> {
    let steam = Steam::find(&Paths::from_env().home)?;
    steamgames::find_app(&steam, app_id).map(|a| a.name)
}

/// Steam games with a mod store (`games/steam-<appid>/profiles` or `mods/steam-<appid>/`),
/// with their names.
pub(crate) fn with_mods() -> Vec<(GameRef, String)> {
    let data = quiver_core::launcher::data_dir();
    let mut ids = std::collections::BTreeSet::new();
    for (dir, sub) in [("games", "profiles"), ("mods", "layers")] {
        for e in std::fs::read_dir(data.join(dir)).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_prefix("steam-").and_then(|i| i.parse::<u32>().ok())
                && e.path().join(sub).is_dir()
            {
                ids.insert(id);
            }
        }
    }
    let steam = Steam::find(&Paths::from_env().home);
    ids.into_iter()
        .map(|id| {
            let name = steam.as_ref().and_then(|s| steamgames::find_app(s, id)).map(|a| a.name).unwrap_or_else(|| format!("steam:{id}"));
            (GameRef::steam(id), name)
        })
        .collect()
}

/// A launch quiverd prepared, until `steam-run` registers its play session.
#[derive(Debug, Clone)]
pub(crate) struct Prepared {
    pub game: GameRef,
    pub profile: Option<String>,
    pub profile_revision: Option<u32>,
    pub mods: Vec<ResolvedMod>,
    pub options_revision: Option<u32>,
    at: Instant,
}

static PREPARED: LazyLock<Mutex<HashMap<String, Prepared>>> = LazyLock::new(Mutex::default);

/// Prepared launches nobody claimed are dropped after this long (the game never started).
const KEEP: Duration = Duration::from_secs(600);

fn remember(id: &str, p: Prepared) {
    let mut m = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, v| v.at.elapsed() < KEEP);
    m.insert(id.to_owned(), p);
}

/// The prepared launch `id`, for the game's play session (taken: it's claimed once).
pub(crate) fn claim(id: &str, game: GameRef) -> Option<Prepared> {
    let mut m = PREPARED.lock().unwrap_or_else(|e| e.into_inner());
    m.remove(id).filter(|p| p.game == game && p.at.elapsed() < KEEP)
}

/// The prefix a launch uses: Steam's compatibility folder's `pfx` when Steam set one for this
/// launch (Proton; Steam may keep it in another library than the game), else none (a native
/// launch), whatever folders are lying around.
pub(crate) fn launch_prefix(compat_data: Option<&str>) -> PathBuf {
    compat_data.map(str::trim).filter(|c| !c.is_empty()).map(|c| PathBuf::from(c).join("pfx")).unwrap_or_default()
}

/// The overlays of a plan, on the wire.
pub(crate) fn overlays(plan: &quiver_core::mods::Plan) -> Vec<SteamOverlay> {
    plan.overlays.iter().map(|o| SteamOverlay { target: o.target.clone(), lower: o.lower.clone(), upper: o.upper.clone(), work: o.work.clone() }).collect()
}

impl Daemon {
    /// `SteamLaunchPlan { mods: true }` for a Steam game: prepare its mods (see the module
    /// docs) and put them in `plan`: what to mount, and the launch settings resolved through
    /// the profile's stack (the game's settings, then each mod's, then the profile's own
    /// overrides) in place of the game's settings alone.
    pub(crate) fn steam_mods_prepare(
        &self,
        source: &str,
        game: GameRef,
        requested: Option<&str>,
        steam_env: &std::collections::BTreeMap<String, String>,
        plan: &mut SteamLaunchPlan,
    ) -> Result<()> {
        let app = game.as_steam().context("not a Steam game")?;
        let mut inst = mod_game(app)?;
        // This launch says whether it's Proton: Steam sets `STEAM_COMPAT_DATA_PATH` for Proton
        // launches only (a native game with an old compatibility folder has no prefix here).
        inst.prefix = launch_prefix(steam_env.get("STEAM_COMPAT_DATA_PATH").map(String::as_str));
        let name = inst.name.clone();
        let chosen_by = match requested {
            Some(_) => "steam-run --profile",
            None => "the game's steam_profile setting",
        };
        let (mut mp, fallback) = crate::mods::launch_plan(&inst, requested, source)?;
        if let Some(w) = &fallback {
            self.log_activity_game(source, format!("{name}: {w}"), Some(game));
        }
        let launch = crate::options::new_session_id();
        mp.session = Some(launch.clone());
        let profile = mp.api.profile.clone();
        let mut out = SteamMods {
            launch: launch.clone(),
            profile: profile.as_ref().map(|p| p.id.clone()),
            profile_name: profile.as_ref().map(|p| p.name.clone()),
            revision: mp.api.revision,
            chosen_by: if profile.is_some() { chosen_by.to_owned() } else { "vanilla".to_owned() },
            resolved: mp.api.resolved.clone(),
            problems: mp.api.problems.clone(),
            lines: crate::mods::plan_lines(&mp.api),
            overlays: Vec::new(),
        };
        let what = match &profile {
            Some(p) => format!("profile '{}' ({}) r{}{}", p.name, p.id, mp.api.revision, if mp.api.resolved.is_empty() { String::new() } else { format!(": {}", crate::mods::mods_line(&mp.api)) }),
            None if !mp.api.mods.is_empty() => format!("vanilla with fixes {}", crate::mods::mods_line(&mp.api)),
            None => "vanilla".to_owned(),
        };
        if !out.problems.is_empty() {
            self.log_activity_game(source, format!("can't start {name} from Steam with {what}: {}", out.problems.join("; ")), Some(game));
            plan.mods = Some(out);
            return Ok(());
        }
        // Options: rendered into the profile's install layer and patched into the prefix (the
        // prefix wants join the config patches below).
        out.lines.extend(self.options_launch(&inst, &mut mp, &launch));
        // Config patches, to the files as the launch will see them, before anything mounts.
        match crate::mods::locked(|| mp.apply_patches()) {
            Ok(st) => out.lines.extend(quiver_core::launcher::describe_patches(&st)),
            Err(e) => out.lines.push(format!("config patches not applied: {e:#}")),
        }
        for o in &mp.overlays {
            // A profile's own prefix folder may not exist yet (the game hasn't run).
            std::fs::create_dir_all(&o.target).with_context(|| format!("creating {}", o.target.display()))?;
        }
        out.overlays = overlays(&mp);
        // The launch settings through the stack (Steam still picks Proton and the language).
        let resolved = crate::steamlib::plan_from(game, mp.api.settings.clone(), plan.revision);
        plan.env = resolved.env;
        plan.dll_overrides = resolved.dll_overrides;
        plan.args = resolved.args;
        plan.wrappers = resolved.wrappers;
        plan.ignored = resolved.ignored;
        remember(
            &launch,
            Prepared {
                game,
                profile: out.profile.clone(),
                profile_revision: profile.as_ref().map(|_| mp.api.revision),
                mods: mp.api.resolved.clone(),
                options_revision: self.options.launched_revision(&launch),
                at: Instant::now(),
            },
        );
        let mounts = if out.overlays.is_empty() { "nothing to mount".to_owned() } else { format!("{} overlay(s) in the launch's namespace", out.overlays.len()) };
        self.log_activity_game(source, format!("Steam launch of {name} with {what} ({}; {mounts})", out.chosen_by), Some(game));
        plan.mods = Some(out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(root: &std::path::Path, app_id: u32) -> AppManifest {
        AppManifest {
            app_id,
            name: "The Witcher 3".into(),
            library: root.to_owned(),
            install_dir: root.join("steamapps/common/The Witcher 3"),
            installed: true,
            owner: None,
            scripts: Vec::new(),
            build_id: None,
            state_flags: 4,
            size_on_disk: 0,
            last_played: None,
            dlc_installed: Vec::new(),
            target_build_id: None,
            download: Default::default(),
        }
    }

    #[test]
    fn a_steam_game_as_mods_see_it() {
        let t = tempfile::tempdir().unwrap();
        // Native: no compatibility folder, so no prefix (and docs/prefix options don't apply).
        let g = game_of(&manifest(t.path(), 292030));
        assert_eq!((g.product_id, g.game_ref(), g.prefix()), (0, GameRef::steam(292030), None));
        assert_eq!(g.install_dir, t.path().join("steamapps/common/The Witcher 3"));
        // Proton: Steam's compatdata prefix, even before its first run made `pfx`.
        std::fs::create_dir_all(t.path().join("steamapps/compatdata/292030")).unwrap();
        let g = game_of(&manifest(t.path(), 292030));
        assert_eq!(g.prefix(), Some(t.path().join("steamapps/compatdata/292030/pfx").as_path()));
        // Its stores are keyed by the Steam ref, never a Ubisoft folder.
        let store = quiver_core::mods::ModStore::new(t.path(), 0).with_game(&g, None);
        assert_eq!(store.game_ref(), GameRef::steam(292030));
        assert_eq!(store.profile_dir("p1"), t.path().join("mods/steam-292030/profiles/p1"));
        assert!(!store.has_save_stores());
        // On disk, a Ubisoft game's record is unchanged; a Steam game's names itself.
        let ubi = Game { product_id: 3539, name: "O".into(), install_dir: "/i".into(), prefix: "/p".into(), exe: "x.exe".into(), args: None, language: None, game: None };
        let v = serde_json::to_value(&ubi).unwrap();
        assert!(v.get("game").is_none());
        assert_eq!(serde_json::to_value(&g).unwrap()["game"], "steam:292030");
    }

    #[test]
    fn the_launch_says_whether_it_is_proton() {
        assert_eq!(launch_prefix(Some("/s/steamapps/compatdata/255710")), std::path::Path::new("/s/steamapps/compatdata/255710/pfx"));
        // A native launch (of a game that may once have run with Proton): no prefix.
        assert_eq!(launch_prefix(None), std::path::Path::new(""));
        assert_eq!(launch_prefix(Some(" ")), std::path::Path::new(""));
    }

    #[test]
    fn prepared_launches_are_claimed_once_by_their_game() {
        let p = |g| Prepared { game: g, profile: Some("p1".into()), profile_revision: Some(2), mods: Vec::new(), options_revision: None, at: Instant::now() };
        remember("s-test1", p(GameRef::steam(1)));
        assert!(claim("s-test1", GameRef::steam(2)).is_none(), "another game's launch");
        remember("s-test1", p(GameRef::steam(1)));
        let c = claim("s-test1", GameRef::steam(1)).unwrap();
        assert_eq!((c.profile.as_deref(), c.profile_revision), (Some("p1"), Some(2)));
        assert!(claim("s-test1", GameRef::steam(1)).is_none(), "claimed once");
    }
}
