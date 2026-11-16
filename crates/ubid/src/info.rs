//! Game detail, progression and saves requests (the game page's Overview, Add-ons and
//! "Saves & progress" tabs). Everything here uses the game's launch account.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ubi_api::info::{GameInfo, Progress, SavesReport};
use ubi_api::Request;
use ubi_core::gameinfo::{self, Associated};
use ubi_core::launcher::Games;
use ubi_core::library::{Library, Product};
use ubi_core::online::Online;
use ubi_core::{progress, saves};

use crate::{Daemon, run_local};

/// How long an account's ownership list (configs, associations, space ids) is reused.
const OWNERSHIP_TTL: Duration = Duration::from_secs(15 * 60);
/// How long progression is reused before asking Ubisoft again.
const PROGRESS_TTL: Duration = Duration::from_secs(5 * 60);
/// Most add-on configs fetched one by one for a single game.
const MAX_CONFIG_FETCHES: usize = 100;

struct Owned {
    at: Instant,
    products: HashMap<u32, Product>,
}

#[derive(Default)]
struct Caches {
    /// Per account (user id).
    owned: HashMap<String, Arc<Owned>>,
    /// Configs of products missing from the ownership list (`None`: Ubisoft had none).
    configs: HashMap<u32, Option<String>>,
    /// Per (account, product).
    progress: HashMap<(String, u32), (Instant, Progress)>,
}

static CACHES: LazyLock<Mutex<Caches>> = LazyLock::new(Mutex::default);

/// Sources whose restores (and other "Ask" actions) need a person's approval.
pub(crate) fn needs_approval(source: &str) -> bool {
    source.to_ascii_lowercase().starts_with("agent")
}

/// `$XDG_CACHE_HOME/quiver` (downloaded assets that can be fetched again).
fn cache_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"))
        .join("quiver")
}

fn user_id(online: &Online) -> Result<String> {
    online.user_id().map(str::to_owned).context("no account for this game")
}

/// Associated products of `product_id` whose config we have neither from the ownership
/// list nor from an earlier fetch.
fn missing_configs(owned: &Owned, product_id: Option<u32>, known: &HashMap<u32, Option<String>>) -> Vec<u32> {
    let Some(game) = product_id.and_then(|id| owned.products.get(&id)) else { return Vec::new() };
    game.associations
        .iter()
        .copied()
        .filter(|id| owned.products.get(id).is_none_or(|p| p.configuration.is_none()) && !known.contains_key(id))
        .take(MAX_CONFIG_FETCHES)
        .collect()
}

/// The account's ownership list, and configs for `product_id`'s associated products that
/// aren't in it. One demux round per cache miss, on the account's shared connection.
async fn ownership(online: &Arc<Online>, product_id: Option<u32>, refresh: bool) -> Result<(Arc<Owned>, HashMap<u32, Option<String>>)> {
    let account = user_id(online)?;
    let (cached, known) = {
        let c = CACHES.lock().unwrap();
        let owned = c.owned.get(&account).filter(|o| !refresh && o.at.elapsed() < OWNERSHIP_TTL).cloned();
        (owned, c.configs.clone())
    };
    if let Some(owned) = &cached
        && missing_configs(owned, product_id, &known).is_empty()
    {
        return Ok((owned.clone(), known));
    }
    let online = online.clone();
    let reuse = cached.clone();
    let (owned, fetched) = run_local(move || async move {
        let demux = online.demux().await?;
        let mut lib = Library::fetch(&demux).await?;
        let owned = match reuse {
            Some(o) => o,
            None => Arc::new(Owned { at: Instant::now(), products: lib.products.iter().map(|p| (p.product_id, p.clone())).collect() }),
        };
        let mut fetched = HashMap::new();
        for id in missing_configs(&owned, product_id, &known) {
            let cfg = match lib.product_config(id).await {
                Ok(c) => Some(c).filter(|c| !c.trim().is_empty()),
                Err(e) => {
                    tracing::debug!(id, "no product config: {e:#}");
                    None
                }
            };
            fetched.insert(id, cfg);
        }
        Ok((owned, fetched))
    })
    .await?;
    let mut c = CACHES.lock().unwrap();
    c.owned.insert(account, owned.clone());
    c.configs.extend(fetched);
    Ok((owned, c.configs.clone()))
}

async fn game_info(daemon: &Arc<Daemon>, product_id: u32, refresh: bool) -> Result<GameInfo> {
    let online = daemon.account_for(product_id, None).await?;
    let entry = daemon.library(refresh).await.ok().and_then(|l| l.into_iter().find(|e| e.product_id == product_id));
    let installed = Games::load()?.get(product_id).cloned();
    let (owned, configs) = ownership(&online, Some(product_id), refresh).await?;
    let product = owned.products.get(&product_id);
    if product.is_none() && entry.is_none() && installed.is_none() {
        bail!("product {product_id} is not in your library");
    }
    let name = entry
        .as_ref()
        .map(|e| e.name.clone())
        .or_else(|| product.and_then(|p| p.name.clone()))
        .or_else(|| installed.as_ref().map(|g| g.name.clone()))
        .unwrap_or_else(|| product_id.to_string());
    let associated: Vec<Associated> = product
        .map(|p| p.associations.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|&id| match owned.products.get(&id) {
            Some(p) => {
                let mut a = Associated::from_product(p);
                a.config = a.config.or_else(|| configs.get(&id).cloned().flatten());
                a
            }
            None => Associated { product_id: id, product_type: None, owned: false, config: configs.get(&id).cloned().flatten() },
        })
        .collect();
    let mut info = gameinfo::build(product_id, &name, product, &associated, installed.as_ref());
    info.account = online.user_id().map(str::to_owned);
    info.owned_by = entry.map(|e| e.owned_by).unwrap_or_default();
    Ok(info)
}

async fn game_progress(daemon: &Arc<Daemon>, product_id: u32, refresh: bool) -> Result<Progress> {
    let online = daemon.account_for(product_id, None).await?;
    let account = user_id(&online)?;
    let key = (account.clone(), product_id);
    if !refresh
        && let Some((at, p)) = CACHES.lock().unwrap().progress.get(&key)
        && at.elapsed() < PROGRESS_TTL
    {
        return Ok(p.clone());
    }
    // The space id is in the ownership data (and the game's config), the achievement set in
    // the config.
    let (owned, _) = ownership(&online, None, false).await?;
    let product = owned.products.get(&product_id);
    let cfg = product.and_then(|p| ubi_core::library::ProductConfig::parse(p.configuration.as_deref()?).ok());
    let refs = progress::GameRefs {
        space_id: product
            .and_then(|p| p.space_id.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| cfg.as_ref()?.root()["space_id"].as_str().map(str::to_owned)),
        achievements_asset: cfg.as_ref().and_then(|c| c.root()["uplay"]["achievements"].as_str().map(str::to_owned)),
    };
    let o = online.clone();
    let mut p = run_local(move || async move {
        let session = o.session().await?;
        let mut p = progress::fetch(&o.auth, &session, &refs, &cache_dir().join("achievements")).await;
        p.product_id = product_id;
        p.account = Some(session.user_id.clone());
        // Ubisoft's play-time record, the headline number (demux `playtime` service).
        match remote_playtime(&o, &session.user_id, product_id).await {
            Ok(Some(s)) if s > 0 => {
                p.playtime_seconds = Some(s);
                p.playtime_source = Some("playtime_service".into());
            }
            Ok(_) => {}
            Err(e) => tracing::info!(product_id, "playtime service: {e:#}"),
        }
        Ok(p)
    })
    .await?;
    if p.playtime_seconds.is_none()
        && let Some(n) = p.stats.iter().find(|s| s.id.eq_ignore_ascii_case("TimePlayed")).and_then(|s| s.number)
    {
        p.playtime_seconds = Some(n as u64);
        p.playtime_source = Some("stat:TimePlayed".into());
    }
    CACHES.lock().unwrap().progress.insert(key, (Instant::now(), p.clone()));
    Ok(p)
}

async fn remote_playtime(online: &Online, user_id: &str, product_id: u32) -> Result<Option<u64>> {
    use ubi_core::proto::playtime as pt;
    let demux = online.demux().await?;
    let mut conn = demux.open::<ubi_core::demux::Playtime>().await?;
    let rsp = conn
        .request(pt::Req {
            get_playtime_req: Some(pt::GetPlaytimeReq { account_id: Some(user_id.to_owned()), product_id: Some(product_id) }),
            ..Default::default()
        })
        .await?;
    let r = rsp.get_playtime_rsp.context("no get_playtime_rsp")?;
    Ok(r.seconds.map(u64::from))
}

async fn saves_report(daemon: &Arc<Daemon>, product_id: u32, cloud: bool) -> Result<SavesReport> {
    let online = daemon.account_for(product_id, None).await?;
    let account = user_id(&online)?;
    let game = Games::load()?.get(product_id).cloned();
    // A Steam copy keeps its saves in Steam's prefix.
    let steam = match &game {
        Some(_) => None,
        None => daemon.library(false).await.ok().and_then(|l| l.into_iter().find(|e| e.product_id == product_id)).and_then(|e| e.steam),
    };
    // An installed game: the save store that syncs with the cloud (Default unless moved).
    let primary = game.as_ref().map(|_| ubi_core::savestores::SaveStores::new(&ubi_core::launcher::data_dir(), product_id).primary());
    let dir = game
        .as_ref()
        .zip(primary.as_ref())
        .map(|(g, s)| ubi_core::savestores::store_dir(&g.prefix, &account, &s.folder))
        .or_else(|| steam.as_ref().and_then(|g| crate::steamlib::steam_save_dir(g, &account, product_id)));
    let mut report =
        SavesReport { product_id, account: Some(account.clone()), dir: dir.clone(), store: primary.map(|s| s.name), ..Default::default() };
    let max_local = dir.as_deref().and_then(|d| saves::local_slots(d).ok()).and_then(|m| m.keys().max().copied()).unwrap_or(0);
    let mut cloud_items = None;
    if cloud {
        // The cloud objects (demux cloudsave_service + signed downloads) and Connect's record
        // of them (one REST GET), side by side.
        let o = online.clone();
        let (objects, record) = run_local(move || async move {
            let objects = async {
                let demux = o.demux().await?;
                let mut lib = Library::fetch(&demux).await?;
                let mut cs = ubi_core::cloudsave::CloudSaves::open(&demux).await?;
                saves::cloud_slots(&mut cs, &mut lib, product_id, max_local).await
            }
            .await;
            let record = async { saves::connect_record(&o.auth, &o.session().await?, product_id).await }.await;
            Ok((objects, record))
        })
        .await?;
        match objects {
            Ok(items) => cloud_items = Some(items),
            Err(e) => report.cloud_error = Some(format!("{e:#}")),
        }
        match record {
            Ok(r) => report.connect_record = r,
            Err(e) => tracing::info!(product_id, "Connect's cloud-save record unavailable: {e:#}"),
        }
    }
    let view = match (&cloud_items, &report.connect_record) {
        (Some(items), _) => {
            report.cloud_source = Some("cloudsave".into());
            saves::CloudView::Objects(items)
        }
        (None, Some(rec)) => {
            report.cloud_source = Some("connect_record".into());
            saves::CloudView::Record(rec)
        }
        (None, None) => saves::CloudView::None,
    };
    // Not installed: only the cloud's slots.
    let local_dir = dir.clone().unwrap_or_else(|| std::path::PathBuf::from("/nonexistent"));
    report.slots = saves::merge(&local_dir, view)?;
    report.backups = saves::list_backups(&daemon.cfg.save_backups, product_id)
        .into_iter()
        .filter(|b| b.account.as_deref().is_none_or(|a| a == account))
        .collect();
    Ok(report)
}

pub(crate) async fn handle(daemon: &Arc<Daemon>, source: &str, req: Request) -> Result<Value> {
    Ok(match req {
        Request::GameInfo { product_id, refresh } => json!(game_info(daemon, product_id, refresh).await?),
        Request::GameProgress { product_id, refresh } => json!(game_progress(daemon, product_id, refresh).await?),
        Request::Saves { product_id, cloud } => json!(saves_report(daemon, product_id, cloud).await?),
        Request::SavesRestore { product_id, backup, confirmed } => {
            let name = daemon.product_name(product_id);
            if needs_approval(source) && !confirmed {
                bail!(
                    "restoring saves needs your approval: run `quiver saves {product_id} restore {backup}` yourself, or restore it from {name}'s Saves tab"
                );
            }
            if daemon.running.lock().unwrap().contains(&product_id)
                || daemon.sessions.list().iter().any(|s| s.product_id == product_id)
            {
                bail!("{name} is running; quit it before restoring saves");
            }
            let game = Games::load()?.get(product_id).cloned().with_context(|| format!("{name} is not installed"))?;
            let online = daemon.account_for(product_id, None).await?;
            let primary = ubi_core::savestores::SaveStores::new(&ubi_core::launcher::data_dir(), product_id).primary();
            let dir = ubi_core::savestores::store_dir(&game.prefix, &user_id(&online)?, &primary.folder);
            let r = saves::restore(&daemon.cfg.save_backups, product_id, &backup, &dir, Some(source))?;
            let what = r.previous.as_ref().map(|p| format!("; previous saves kept as {}", p.id)).unwrap_or_default();
            daemon.log_activity(
                source,
                format!("restored {name} save backup {backup} (slots {:?}){what}{}", r.restored, if confirmed { " (approved)" } else { "" }),
                Some(product_id),
            );
            json!(r)
        }
        Request::SavesSync { product_id, profile } => {
            let name = daemon.product_name(product_id);
            if daemon.running.lock().unwrap().contains(&product_id)
                || daemon.sessions.list().iter().any(|s| s.product_id == product_id)
            {
                bail!("{name} is running; its saves upload when it exits");
            }
            let game = Games::load()?.get(product_id).cloned().with_context(|| format!("{name} is not installed"))?;
            // The save store the profile (or vanilla) uses; only the cloud store syncs.
            let q = profile.as_deref().filter(|q| !q.eq_ignore_ascii_case("vanilla"));
            let plan = ubi_core::mods::ModStore::open(product_id).with_game(&game, None).plan(q)?;
            if !plan.api.saves.cloud {
                bail!("{}", plan.api.saves.cloud_note.clone().unwrap_or_else(|| "these saves aren't synced with the cloud".into()));
            }
            let profile_id = plan.api.profile.as_ref().map(|p| p.id.clone());
            let online = daemon.account_for(product_id, None).await?;
            let user = user_id(&online)?;
            let dir = game.prefix.join(ubi_core::launcher::UPC_REL).join(format!("savegames/{user}/{}", plan.saves_folder));
            let report = run_local(move || async move {
                let demux = online.demux().await?;
                let mut lib = Library::fetch(&demux).await?;
                let mut cloud = ubi_core::cloudsave::CloudSaves::open(&demux).await?;
                ubi_core::savesync::push(&mut cloud, &mut lib, product_id, &dir).await
            })
            .await?;
            daemon.log_activity(
                source,
                format!(
                    "uploaded {name} saves{} (slots {:?})",
                    profile_id.map(|p| format!(" for profile {p}")).unwrap_or_default(),
                    report.uploaded
                ),
                Some(product_id),
            );
            json!(report.uploaded)
        }
        other => bail!("not an info request: {other:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_sources() {
        assert!(needs_approval("agent"));
        assert!(needs_approval("agent:claude-code"));
        assert!(needs_approval("Agent"));
        assert!(!needs_approval("cli"));
        assert!(!needs_approval("gui"));
    }
}
