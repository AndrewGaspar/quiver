//! The daemon's store providers (`quiver_core::stores`) and the requests routed to them.
//!
//! Ubisoft's provider is the code quiverd had before providers, behind the trait: its
//! library (`steamlib::library`), game info, play sessions and launcher. Other stores'
//! providers live in `quiver-core` (Steam: `steamlocal::SteamLocal`).

use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use quiver_api::stores::{ClientInstall, GameDetail, StoreInfo, StorePlaytime};
use quiver_api::{Event, GameRef, JobKind, LibraryEntry, Playtime, StoreId};
use quiver_core::launcher::Games;
use quiver_core::settings;
use quiver_core::stores::{AccountInfo, AccountState, BoxFuture, Capabilities, Launched, StoreProvider, Support, provider_for};

use crate::{Daemon, info, steamlib, ubisoft_only};

/// Ubisoft, through the daemon (it owns the accounts, cache, jobs and launcher). Made per
/// request, so the daemon doesn't hold itself.
pub(crate) struct Ubisoft(pub Arc<Daemon>);

impl StoreProvider for Ubisoft {
    fn id(&self) -> StoreId {
        StoreId::Ubisoft
    }

    fn capabilities(&self) -> Capabilities {
        let q = Support::Quiver;
        Capabilities { library: q, install: q, launch: q, settings: q, mods: q, cloud_saves: q, achievements: q, playtime: q }
    }

    fn account(&self) -> AccountInfo {
        let (accounts, name) = self.0.account_list();
        let state = if accounts.is_empty() { AccountState::NeedsSignIn } else { AccountState::SignedIn };
        AccountInfo { state, name, note: None }
    }

    fn library(&self, refresh: bool) -> BoxFuture<'_, Result<Vec<LibraryEntry>>> {
        Box::pin(steamlib::library(&self.0, refresh))
    }

    fn detail(&self, game: GameRef, refresh: bool) -> BoxFuture<'_, Result<GameDetail>> {
        Box::pin(async move {
            let id = ubisoft_only(game)?;
            let i = info::game_info(&self.0, id, refresh).await?;
            let playtime = settings::playtime(id).ok().map(|p| StorePlaytime { total_seconds: p.total_seconds, two_weeks_seconds: None, last_played: p.last_played });
            let install = i.install.as_ref().map(|x| ClientInstall { dir: x.dir.clone(), size_bytes: x.size_bytes, complete: true, ..Default::default() });
            Ok(GameDetail {
                game,
                name: i.name,
                capabilities: self.capabilities(),
                kind: Some("game".into()),
                developer: i.developer,
                publisher: i.publisher,
                release_date: i.release_date.as_deref().and_then(parse_date),
                store_url: None,
                install,
                playtime,
                achievements: None,
                cloud: None,
                dlc: Vec::new(),
                account: i.account,
                unavailable: i.unavailable,
                steam_hook: None,
                description: None,
                linked: None,
            })
        })
    }

    fn playtime(&self, game: GameRef) -> BoxFuture<'_, Result<Playtime>> {
        Box::pin(async move { settings::playtime(ubisoft_only(game)?) })
    }

    fn launch<'a>(&'a self, source: &'a str, game: GameRef, profile: Option<String>) -> BoxFuture<'a, Result<Launched>> {
        Box::pin(async move {
            let d = &self.0;
            let product_id = ubisoft_only(game)?;
            // A copy only Steam installed: Steam starts it (through Quiver once switched).
            if Games::load()?.get(product_id).is_none()
                && let Some(e) = d.library(false).await?.into_iter().find(|e| e.game == game && e.steam_only())
            {
                return Ok(Launched::Job(steamlib::play(d, source, &e).await?));
            }
            let online = d.account_for(product_id, None).await?;
            Ok(Launched::Job(d.start_play(source, product_id, profile, online)?))
        })
    }
}

/// Release dates as product configs and stores write them: RFC 3339 or `YYYY-MM-DD`.
fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0).map(|t| t.and_utc()))
}

impl Daemon {
    /// Every store provider, Ubisoft first.
    pub(crate) fn providers(self: &Arc<Self>) -> Vec<Arc<dyn StoreProvider>> {
        let mut v: Vec<Arc<dyn StoreProvider>> = vec![Arc::new(Ubisoft(self.clone()))];
        v.extend(self.stores.iter().cloned());
        v
    }

    /// The library from every store, merged on explicit links only (`stores::merge_libraries`).
    /// A store that fails (Ubisoft signed out) doesn't hide the others' games; when every
    /// store fails or has nothing, Ubisoft's error is the answer, as before.
    pub(crate) async fn library_all(self: &Arc<Self>, refresh: bool) -> Result<Vec<LibraryEntry>> {
        let mut parts = Vec::new();
        let mut first_err = None;
        for p in self.providers() {
            match p.library(refresh).await {
                Ok(l) => parts.push(l),
                Err(e) => {
                    tracing::info!(store = %p.id(), "library: {e:#}");
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) if parts.iter().all(Vec::is_empty) => Err(e),
            _ => Ok(quiver_core::stores::merge_libraries(parts)),
        }
    }

    /// `Request::Play`, routed by store. A delegated launch (the store's client plays it)
    /// gets a finished Play job and an activity line here, the same for every store.
    pub(crate) async fn play(self: &Arc<Self>, source: &str, game: GameRef, profile: Option<String>) -> Result<u64> {
        let providers = self.providers();
        let p = provider_for(&providers, &game)?;
        if !game.is_ubisoft() && profile.is_some() {
            anyhow::bail!("{game}: {} plays it as it is; mod profiles don't apply", game.store.label());
        }
        match p.launch(source, game, profile).await? {
            Launched::Job(id) => Ok(id),
            Launched::Delegated { url, argv } => {
                let name = self.game_name(game).await;
                let label = game.store.label();
                self.log_activity_game(source, format!("play {name} from {label}"), Some(game));
                tracing::info!(%game, "asked {label} to start it: {}", argv.join(" "));
                let id = self.new_game_job(JobKind::Play, game, name);
                self.update_job(id, |j| {
                    j.status = format!("Asked {label} to start it");
                    j.detail = vec![url];
                    j.done = true;
                });
                self.emit(Event::LibraryChanged);
                Ok(id)
            }
        }
    }

    pub(crate) async fn game_name(self: &Arc<Self>, game: GameRef) -> String {
        match game.as_ubisoft() {
            Some(id) => self.product_name(id),
            None => self.library_all(false).await.ok().and_then(|l| l.into_iter().find(|e| e.game == game)).map(|e| e.name).unwrap_or_else(|| game.to_string()),
        }
    }

    pub(crate) async fn playtime(self: &Arc<Self>, game: GameRef) -> Result<Playtime> {
        let providers = self.providers();
        provider_for(&providers, &game)?.playtime(game).await
    }

    pub(crate) async fn game_detail(self: &Arc<Self>, game: GameRef, refresh: bool) -> Result<GameDetail> {
        let providers = self.providers();
        let mut d = provider_for(&providers, &game)?.detail(game, refresh).await?;
        // A Steam game: whether it starts through `quiver steam-run` (read-only).
        if let Some(app_id) = game.as_steam() {
            d.steam_hook = tokio::task::spawn_blocking(move || crate::steamhook::status(app_id, None)).await?.map_err(|e| tracing::debug!(%game, "hook status: {e:#}")).ok();
        }
        Ok(d)
    }

    /// `Request::Stores`: each provider, its account and how many games it brings.
    pub(crate) async fn store_list(self: &Arc<Self>) -> Result<Vec<StoreInfo>> {
        let lib = self.library_all(false).await.unwrap_or_default();
        Ok(self
            .providers()
            .iter()
            .map(|p| {
                let a = p.account();
                StoreInfo {
                    store: p.id(),
                    account: a.state,
                    account_name: a.name,
                    capabilities: p.capabilities(),
                    // With copies folded into another store's entry (an Epic copy of a Ubisoft game).
                    games: lib.iter().filter(|e| e.store() == p.id() || e.records.iter().any(|r| r.relation == quiver_api::stores::Relation::CopyOf && r.game.store == p.id())).count(),
                    note: a.note,
                }
            })
            .collect())
    }
}

/// `Request::Stop` for another store's game: its client runs it.
pub(crate) fn stop_elsewhere(game: GameRef) -> Result<()> {
    let label = game.store.label();
    anyhow::bail!("{game} runs in {label}; quit it from the game or {label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_dates() {
        assert_eq!(parse_date("2017-10-27").map(|d| d.to_rfc3339()), Some("2017-10-27T00:00:00+00:00".into()));
        assert_eq!(parse_date("2017-10-27T12:00:00Z").map(|d| d.to_rfc3339()), Some("2017-10-27T12:00:00+00:00".into()));
        assert_eq!(parse_date("soon"), None);
    }
}
