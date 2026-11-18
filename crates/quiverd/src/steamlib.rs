//! Steam copies of Ubisoft games as library entries.
//!
//! A Ubisoft game bought on Steam is activated on the Ubisoft account, so
//! `ownership_service` lists it (with its space id, app id, configuration and add-ons), but
//! with no `latest_manifest`: Ubisoft has no PC build to download for it, Steam does. The
//! library used to drop every product without a PC build, so these games only showed up
//! under Steam games, as "not in your Ubisoft library".
//!
//! Now the daemon's cached library keeps owned games without a PC build
//! (`installable: false`), and the library it hands out lists those of them Steam has
//! installed, plus every installable one, each with its Steam install (`steam`) when there
//! is one. A Steam install no signed-in account owns is listed too (`owned_by` empty), named
//! and drawn from its product configuration. Steam plays (`steam://rungameid/<appid>`),
//! installs, updates and uninstalls them.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use quiver_api::exports::{SteamGame, SteamGames};
use quiver_api::steamlib::{OwnedProduct, SteamLaunchPlan};
use quiver_api::{Art, Event, GameRef, JobKind, LibraryEntry, Request};
use quiver_core::exports::Paths;
use quiver_core::exports::steam::Steam;
use quiver_core::library::{Library, Product, ProductConfig};
use quiver_core::online::Online;
use quiver_core::{settings, store};

use crate::{Daemon, run_local};

/// Why an owned product isn't one of the cached library's games. `None`: it is (a game,
/// owned, not locked behind a lapsed subscription), with or without a PC build.
pub(crate) fn excluded(p: &Product) -> Option<&'static str> {
    if !p.owned {
        Some("not owned")
    } else if p.product_type != 0 {
        Some("not a game (add-on, bundle, season pass, …)")
    } else if p.locked_by_subscription {
        Some("locked by a subscription that ended")
    } else {
        None
    }
}

/// Name, art and space id from a product configuration.
#[derive(Debug, Clone, Default)]
pub(crate) struct Meta {
    name: Option<String>,
    art: Art,
    space_id: Option<String>,
}

fn config_meta(config: &str) -> Option<Meta> {
    let c = ProductConfig::parse(config).ok()?;
    let art = Art {
        thumb: c.art("thumb_image"),
        background: c.art("background_image"),
        logo: c.art("logo_image"),
        icon: c.art("icon_image"),
        ..Default::default()
    };
    let space_id = c.root()["space_id"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    Some(Meta { name: c.name(), art, space_id })
}

/// A library entry for a product from the ownership list.
pub(crate) fn entry_for(p: &Product, installed: Option<quiver_api::Game>, update_available: bool, user_id: &str) -> LibraryEntry {
    let meta = p.configuration.as_deref().and_then(config_meta).unwrap_or_default();
    let space_id = p.space_id.clone().filter(|s| !s.is_empty()).or(meta.space_id);
    let art = meta.art;
    LibraryEntry {
        game: GameRef::ubisoft(p.product_id),
        name: p.name.clone().unwrap_or_else(|| p.product_id.to_string()),
        update_available,
        installed,
        running: false,
        art,
        owned_by: vec![user_id.to_owned()],
        installable: p.is_installable(),
        steam: None,
        space_id,
        client: None,
    }
}

/// Configs (name, art) of Steam installs that no signed-in account owns. `None`: Ubisoft
/// had none.
static UNOWNED: LazyLock<Mutex<HashMap<u32, Option<Meta>>>> = LazyLock::new(Mutex::default);

/// The public library: the cached games with a PC build, those without one that Steam has
/// installed, and Steam installs nobody owns; each with its Steam install.
pub(crate) fn merge(owned: Vec<LibraryEntry>, steam: &[SteamGame], unowned: &HashMap<u32, Option<Meta>>) -> Vec<LibraryEntry> {
    let mut by_product: HashMap<u32, &SteamGame> = HashMap::new();
    for g in steam {
        if let Some(id) = g.product_id {
            by_product.entry(id).or_insert(g);
        }
    }
    let mut seen = HashSet::new();
    let mut out: Vec<LibraryEntry> = owned
        .into_iter()
        .filter_map(|mut e| {
            let id = e.ubisoft_id()?;
            e.steam = by_product.get(&id).map(|g| (*g).clone());
            seen.insert(id);
            (e.installable || e.steam.is_some() || e.installed.is_some()).then_some(e)
        })
        .collect();
    for g in steam {
        let Some(id) = g.product_id else { continue };
        if !seen.insert(id) {
            continue;
        }
        let Meta { name, art, space_id } = unowned.get(&id).cloned().flatten().unwrap_or_default();
        out.push(LibraryEntry {
            game: GameRef::ubisoft(id),
            name: name.unwrap_or_else(|| g.name.clone()),
            installed: None,
            update_available: false,
            running: false,
            art,
            owned_by: Vec::new(),
            installable: false,
            steam: Some(g.clone()),
            space_id,
            client: None,
        });
    }
    out.sort_by(|a, b| {
        let on_disk = |e: &LibraryEntry| e.installed.is_some() || e.steam.is_some();
        on_disk(b).cmp(&on_disk(a)).then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Ubisoft games Steam has installed (`Request::SteamGames`, answered against the cached
/// library so it can say which ones an account owns). Empty without Steam.
async fn steam_games(daemon: &Arc<Daemon>) -> Vec<SteamGame> {
    match crate::steamgames::handle(daemon, "system", Request::SteamGames { steam_user: None }).await {
        Ok(v) => serde_json::from_value::<SteamGames>(v).map(|g| g.games).unwrap_or_default(),
        Err(e) => {
            tracing::debug!("Steam games for the library: {e:#}");
            Vec::new()
        }
    }
}

/// Fetch the configs of Steam installs missing from every account's ownership list.
async fn fetch_unowned(daemon: &Arc<Daemon>, ids: Vec<u32>) {
    if ids.is_empty() {
        return;
    }
    let Ok(online) = daemon.accounts.get(None) else { return };
    let fetched = run_local(move || async move {
        let demux = online.demux().await?;
        let mut lib = Library::fetch(&demux).await?;
        let mut out = HashMap::new();
        for id in ids {
            let meta = match lib.product_config(id).await {
                Ok(c) => config_meta(&c),
                Err(e) => {
                    tracing::info!(product_id = id, "no product config for a Steam install: {e:#}");
                    None
                }
            };
            out.insert(id, meta);
        }
        Ok(out)
    })
    .await;
    match fetched {
        Ok(m) => UNOWNED.lock().unwrap().extend(m),
        Err(e) => tracing::info!("product configs for Steam installs: {e:#}"),
    }
}

/// The library with Steam installs (see the module docs).
pub(crate) async fn library(daemon: &Arc<Daemon>, refresh: bool) -> Result<Vec<LibraryEntry>> {
    let owned = daemon.owned_library(refresh).await?;
    let steam = steam_games(daemon).await;
    let missing: Vec<u32> = {
        let known = UNOWNED.lock().unwrap();
        let ids: HashSet<u32> = owned.iter().filter_map(|e| e.ubisoft_id()).collect();
        steam.iter().filter_map(|g| g.product_id).filter(|id| !ids.contains(id) && (refresh || !known.contains_key(id))).collect()
    };
    fetch_unowned(daemon, missing).await;
    let unowned = UNOWNED.lock().unwrap().clone();
    let mut out = merge(owned, &steam, &unowned);
    add_space_art(daemon, &mut out, refresh).await;
    // A Steam copy with a play session is running (Steam started it). Quiver's own
    // installs keep `running` for games quiverd started, which it can stop.
    let playing: HashSet<u32> = daemon.sessions.list().iter().map(|s| s.product_id).collect();
    for e in out.iter_mut().filter(|e| e.steam_only()) {
        e.running = e.ubisoft_id().is_some_and(|id| playing.contains(&id));
    }
    Ok(out)
}

/// Whether the config's art leaves a slot empty (games bought on Steam have none).
fn needs_space_art(e: &LibraryEntry) -> bool {
    let a = &e.art;
    a.thumb.is_none() || a.background.is_none() || a.logo.is_none()
}

/// Art from UbiServices' games API for entries whose configs leave slots empty (see
/// `quiver_api::art` for the order front ends try sources in). Cached for a day; fails soft.
async fn add_space_art(daemon: &Arc<Daemon>, entries: &mut [LibraryEntry], refresh: bool) {
    let ids: Vec<String> = entries.iter().filter(|e| needs_space_art(e)).filter_map(|e| e.space_id.clone()).collect();
    if ids.is_empty() {
        return;
    }
    let found = crate::spaces::games(daemon, ids, refresh).await;
    for e in entries.iter_mut().filter(|e| needs_space_art(e)) {
        if let Some(g) = e.space_id.as_ref().and_then(|s| found.get(s)) {
            e.art.space = g.art();
        }
    }
}

/// `Request::Play` for a game only Steam has installed: ask Steam to start it. Returns a
/// finished Play job (the session shows up when the game's bridge reports in).
pub(crate) async fn play(daemon: &Arc<Daemon>, source: &str, entry: &LibraryEntry) -> Result<u64> {
    let g = entry.steam.clone().context("not installed through Steam")?;
    let product_id = entry.ubisoft_id().context("not a Ubisoft game")?;
    let app_id = g.app_id;
    let argv = tokio::task::spawn_blocking(move || {
        let steam = Steam::find(&Paths::from_env().home).context("Steam not found")?;
        quiver_core::exports::steamlaunch::launch(&steam, app_id)
    })
    .await??;
    let via = if g.state.uses_quiver() { "through Quiver" } else { "through Ubisoft Connect (it isn't switched to Quiver)" };
    daemon.log_activity(source, format!("play {} from Steam (app {app_id}, {via})", entry.name), Some(product_id));
    tracing::info!(product_id, app_id, "asked Steam to start it: {}", argv.join(" "));
    let id = daemon.new_job(JobKind::Play, product_id, entry.name.clone());
    daemon.update_job(id, |j| {
        j.status = "Asked Steam to start it".into();
        j.detail = vec![format!("{} ({via})", quiver_core::exports::steamlaunch::rungameid_url(app_id))];
        j.done = true;
    });
    daemon.emit(Event::LibraryChanged);
    Ok(id)
}

fn owned_product(p: &Product, steam_installed: bool) -> OwnedProduct {
    let g = &p.raw;
    let because = excluded(p).or_else(|| (!p.is_installable() && !steam_installed).then_some("no Ubisoft PC build, and Steam hasn't installed it"));
    OwnedProduct {
        product_id: p.product_id,
        name: p.name.clone(),
        product_type: p.product_type,
        state: p.state,
        owned: p.owned,
        has_pc_build: p.is_installable(),
        download_id: p.download_id,
        platform: g.platform,
        space_id: p.space_id.clone().filter(|s| !s.is_empty()),
        app_id: p.app_id.clone().filter(|s| !s.is_empty()),
        locked_by_subscription: p.locked_by_subscription,
        activation_type: g.activation_type,
        target_partner: g.target_partner,
        package_ownership_state: g.package_ownership_state,
        brand_id: g.brand_id,
        associations: p.associations.clone(),
        has_configuration: p.configuration.is_some(),
        listed: because.is_none(),
        not_listed_because: because.map(str::to_owned),
    }
}

async fn ownership(daemon: &Arc<Daemon>, product_id: Option<u32>, account: Option<String>) -> Result<Vec<OwnedProduct>> {
    let online: Arc<Online> = match (&account, product_id) {
        (Some(_), Some(id)) => daemon.account_for(id, account.as_deref()).await?,
        (Some(q), None) => daemon.accounts.get(Some(&store::resolve(q)?.user_id))?,
        (None, Some(id)) => daemon.account_for(id, None).await?,
        (None, None) => daemon.accounts.get(None)?,
    };
    let steam: HashSet<u32> = steam_games(daemon).await.iter().filter_map(|g| g.product_id).collect();
    let products = run_local(move || async move {
        let demux = online.demux().await?;
        Ok(Library::fetch(&demux).await?.products)
    })
    .await?;
    Ok(products
        .iter()
        .filter(|p| product_id.is_none_or(|id| p.product_id == id))
        .map(|p| owned_product(p, steam.contains(&p.product_id)))
        .collect())
}

/// Steam's prefix for a Steam copy: `<library>/steamapps/compatdata/<appid>/pfx`, next to
/// `<library>/steamapps/common/<game>`.
pub(crate) fn steam_prefix(g: &SteamGame) -> Option<std::path::PathBuf> {
    let steamapps = g.install_dir.parent()?.parent()?;
    Some(steamapps.join("compatdata").join(g.app_id.to_string()).join("pfx"))
}

/// The SDK saves folder of a Steam copy on the host, where Connect (and `quiver steam-run`)
/// keep its saves: `<Connect InstallDir>/savegames/<user id>/<product>` in Steam's prefix.
pub(crate) fn steam_save_dir(g: &SteamGame, user_id: &str, product_id: u32) -> Option<std::path::PathBuf> {
    let prefix = steam_prefix(g)?;
    let reg = std::fs::read_to_string(prefix.join("system.reg")).unwrap_or_default();
    let install = quiver_core::steamrun::check_sdk(&prefix, &reg).install_dir;
    let win = quiver_core::steamrun::savegame_path(install.as_deref(), user_id, product_id);
    quiver_core::steamrun::host_path(&prefix, &win)
}

/// The launch settings `quiver steam-run` applies (see `SteamLaunchPlan`): a Ubisoft
/// game's Steam copy's, or any other Steam game's (`steam:<appid>`, its own settings).
pub(crate) fn launch_plan(game: GameRef) -> Result<SteamLaunchPlan> {
    if game.as_ubisoft().is_none() && game.as_steam().is_none() {
        anyhow::bail!("{game} isn't a Steam game: Steam launches only Steam games and Steam copies of Ubisoft games");
    }
    let (s, revision) = settings::current(game)?;
    Ok(plan_from(game, s, revision))
}

fn plan_from(game: GameRef, s: quiver_api::GameSettings, revision: u32) -> SteamLaunchPlan {
    let mut ignored = Vec::new();
    if let Some(p) = &s.proton {
        ignored.push(format!("proton={p} (Steam's compatibility setting picks Proton)"));
    }
    if let Some(l) = &s.language {
        ignored.push(format!("language={l} (Steam's language setting decides)"));
    }
    SteamLaunchPlan { revision, env: s.env, dll_overrides: s.dll_overrides, args: s.args, wrappers: s.wrappers, ignored, ..SteamLaunchPlan::for_game(game) }
}

/// The plan of a Steam launch: settings, then tweaks (they see the settings' environment
/// and lose to it per key), then with `perf` the HUD's session and Sight layer (which see
/// both, so the layer order accounts for vkBasalt or lsfg-vk the settings turn on). Shared by
/// Steam copies of Ubisoft games and every other Steam game.
///
/// `steam_env`: the environment Steam set for the launch (`SteamLaunchPlan`'s `env`), which
/// `steam-run` keeps: tweaks yield to it per key, and the layer order counts its layers.
pub(crate) fn full_plan(daemon: &Daemon, game: GameRef, perf: bool, steam_env: &std::collections::BTreeMap<String, String>) -> Result<SteamLaunchPlan> {
    let mut plan = launch_plan(game)?;
    // Quiver Tweaks: the same compiled environment a Quiver launch gets (Steam picks the
    // Proton build; design/TWEAKS.md §8.1).
    crate::tweaks::steam_plan(&daemon.cfg, &mut plan, steam_env);
    if perf {
        daemon.perf_steam_launch(&mut plan, steam_env);
    }
    Ok(plan)
}

impl Daemon {
    /// `SettingsSet` for a Steam game (not a Ubisoft product): the settings `quiver steam-run`
    /// applies to its Steam launch. Ubisoft-only keys are refused.
    pub(crate) fn steam_settings_set(&self, source: &str, game: GameRef, ops: Vec<quiver_api::SettingOp>, message: Option<String>) -> Result<Value> {
        for op in &ops {
            let key = match op {
                quiver_api::SettingOp::Set { key, .. } | quiver_api::SettingOp::Unset { key } => key.as_str(),
            };
            if matches!(key, "account" | "profile") {
                anyhow::bail!("{game}: `{key}` is for Ubisoft games (Steam plays it with the Steam account, without mod profiles)");
            }
        }
        let rev = settings::apply(game, &ops, source, message)?;
        let hooked = crate::steamhook::hooked(game).unwrap_or(false);
        let note = if hooked { "" } else { " (not hooked: they apply once it starts through quiver steam-run)" };
        self.log_activity_game(source, format!("settings r{} for {game}{note}", rev.revision), Some(game));
        Ok(json!(rev))
    }
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, _source: &str, req: Request) -> Result<Value> {
    Ok(match req {
        Request::Ownership { product_id, account } => json!(ownership(daemon, product_id, account).await?),
        Request::SteamLaunchPlan { game, perf, env } => json!(full_plan(daemon, *game, perf, &env)?),
        other => anyhow::bail!("not a Steam library request: {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use quiver_api::exports::PatchState;

    fn entry(id: u32, name: &str, installable: bool) -> LibraryEntry {
        LibraryEntry {
            game: quiver_api::GameRef::ubisoft(id),
            name: name.into(),
            installed: None,
            update_available: false,
            running: false,
            art: Art::default(),
            owned_by: vec!["me".into()],
            installable,
            steam: None,
            space_id: None,
            client: None,
        }
    }

    fn steam(app_id: u32, product: Option<u32>, name: &str) -> SteamGame {
        SteamGame {
            app_id,
            name: name.into(),
            install_dir: format!("/steam/common/{name}").into(),
            product_id: product,
            product_from: Some("install_script".into()),
            owned: Some(true),
            steam_user: Some("1".into()),
            launch_options: String::new(),
            state: PatchState::Off,
            patchable: true,
            offer: false,
            note: None,
            art: Default::default(),
        }
    }

    #[test]
    fn steam_copies_join_the_library() {
        let owned = vec![
            entry(3539, "Assassin's Creed Origins", true),
            entry(66088, "Assassin's Creed Black Flag Resynced", false),
            entry(777, "Owned, no PC build, not installed", false),
        ];
        let steam_games = [
            steam(3751950, Some(66088), "Black Flag Resynced"),
            steam(582160, Some(3539), "Origins on Steam"),
            steam(1, None, "Unknown Ubisoft game"),
            steam(2, Some(5000), "Not on any account"),
        ];
        let mut unowned = HashMap::new();
        unowned.insert(
            5000,
            Some(Meta { name: Some("Far Cry Thing".to_owned()), art: Art { thumb: Some("t.jpg".into()), ..Default::default() }, space_id: Some("s-1".into()) }),
        );
        let lib = merge(owned, &steam_games, &unowned);
        let ids: Vec<u32> = lib.iter().filter_map(|e| e.ubisoft_id()).collect();
        // Installed (Steam or Quiver) first, by name; the uninstalled game without a build is gone.
        assert_eq!(ids, [66088, 3539, 5000]);
        let bf = &lib[0];
        assert!(bf.steam_only() && !bf.installable && bf.steam.as_ref().unwrap().app_id == 3751950);
        assert_eq!(bf.owned_by, ["me"]);
        // An installable game also installed through Steam keeps both facts.
        assert!(lib[1].installable && lib[1].steam.is_some());
        // Not owned by any signed-in account: named from its config, no owners.
        assert_eq!(lib[2].name, "Far Cry Thing");
        assert!(lib[2].owned_by.is_empty() && lib[2].art.thumb.is_some());
        assert_eq!(lib[2].space_id.as_deref(), Some("s-1"));

        // Without Steam, the old library: installable games only.
        let lib = merge(vec![entry(3539, "O", true), entry(66088, "BF", false)], &[], &HashMap::new());
        assert_eq!(lib.iter().filter_map(|e| e.ubisoft_id()).collect::<Vec<_>>(), [3539]);
        // A Steam install whose config couldn't be fetched uses Steam's name.
        let lib = merge(vec![], &[steam(9, Some(9), "Steam Name")], &HashMap::new());
        assert_eq!(lib[0].name, "Steam Name");
    }

    #[test]
    fn steam_save_dir_is_in_steams_prefix() {
        let g = steam(3751950, Some(66088), "BF");
        let dir = steam_save_dir(&g, "uid", 66088).unwrap();
        assert_eq!(
            dir,
            std::path::Path::new("/steam/compatdata/3751950/pfx/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/savegames/uid/66088")
        );
    }

    #[test]
    fn launch_plan_reports_what_steam_decides() {
        let mut s = quiver_api::GameSettings::default();
        for op in ["proton=GE-Proton10-34", "env.DXVK_HUD=fps", "dll.dxgi=n,b", "args=-skipintro", "wrappers=gamemoderun", "account=x"] {
            s.apply(&quiver_api::SettingOp::parse(op).unwrap()).unwrap();
        }
        let p = plan_from(GameRef::ubisoft(66088), s, 4);
        assert_eq!((p.product_id, p.revision), (66088, 4));
        assert_eq!(p.env["DXVK_HUD"], "fps");
        assert_eq!(p.dll_overrides["dxgi"], "n,b");
        assert_eq!(p.args, ["-skipintro"]);
        assert_eq!(p.wrappers, ["gamemoderun"]);
        assert_eq!(p.ignored.len(), 1);
        assert!(p.ignored[0].starts_with("proton=GE-Proton10-34"));
        assert!(plan_from(GameRef::ubisoft(1), Default::default(), 0).ignored.is_empty());
        // Any other Steam game: the same plan from its own settings, product 0.
        let mut s = quiver_api::GameSettings::default();
        for op in ["env.PROTON_LOG=1", "wrappers=gamemoderun", "args=-dx12"] {
            s.apply(&quiver_api::SettingOp::parse(op).unwrap()).unwrap();
        }
        let p = plan_from(GameRef::steam(870780), s, 2);
        assert_eq!((p.product_id, p.game, p.game_ref()), (0, Some(GameRef::steam(870780)), GameRef::steam(870780)));
        assert_eq!((p.env["PROTON_LOG"].as_str(), p.wrappers.as_slice(), p.args.as_slice()), ("1", &["gamemoderun".to_owned()][..], &["-dx12".to_owned()][..]));
        let wire = serde_json::to_value(&p).unwrap();
        assert_eq!(wire["game"], "steam:870780");
        assert!(launch_plan(GameRef::parse("epic:Camellia").unwrap()).is_err());
    }
}
