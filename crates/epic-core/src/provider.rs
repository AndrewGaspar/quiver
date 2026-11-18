//! Epic as a store provider (design/STORES.md §3): `epic:<app name>` games in the merged
//! library, their pages, and Play refused with the reason until phase 2 installs them.
//!
//! A Ubisoft game sold on Epic is listed with a [`Relation::Linked`] record naming its
//! Ubisoft product, so `quiver_core::stores::merge_libraries` folds it into that Ubisoft
//! entry (with an "Epic" badge) when a signed-in Ubisoft account has it, and keeps it as
//! an Epic entry ("not on your Ubisoft account yet") when none does.

use std::sync::Arc;

use anyhow::{Result, bail};
use quiver_api::art::SpaceArt;
use quiver_api::stores::{Dlc, GameDetail, Relation, StoreRecord};
use quiver_api::{Art, Game, GameRef, LibraryEntry, Playtime, StoreId};
use quiver_core::stores::{AccountInfo, AccountState, BoxFuture, Capabilities, Launched, StoreProvider, Support};

use crate::Epic;
use crate::installs::Installed;
use crate::library::Title;

pub struct EpicProvider {
    epic: Arc<Epic>,
}

impl EpicProvider {
    pub fn new(epic: Arc<Epic>) -> Self {
        EpicProvider { epic }
    }
}

pub fn capabilities() -> Capabilities {
    // Quiver installs and launches Epic games, syncs launcher cloud saves and reports play
    // time; achievements are read.
    Capabilities {
        library: Support::Quiver,
        install: Support::Quiver,
        launch: Support::Quiver,
        settings: Support::Quiver,
        mods: Support::Quiver,
        cloud_saves: Support::Quiver,
        achievements: Support::ReadOnly,
        playtime: Support::Quiver,
    }
}

/// `epic:<app>`, when the app name is a valid game id.
pub fn game_ref(app_name: &str) -> Option<GameRef> {
    GameRef::new(StoreId::Epic, app_name).ok()
}

fn ubisoft_of(t: &Title) -> Option<GameRef> {
    t.third_party.as_ref().filter(|p| p.is_ubisoft()).and_then(|p| p.ubisoft_product).map(GameRef::ubisoft)
}

/// A Quiver install as the launcher, mods and settings see a game: Epic's folder and
/// prefix, the manifest's launch executable and arguments.
pub fn game_of(i: &Installed) -> Option<Game> {
    Some(Game {
        product_id: 0,
        name: i.title.clone(),
        install_dir: i.dir.clone(),
        prefix: i.prefix.clone(),
        exe: i.exe.clone(),
        args: None,
        language: None,
        game: Some(game_ref(&i.app_name)?),
    })
}

/// The library entry for a title, installed or not.
pub fn entry(t: &Title, installed: Option<&Installed>) -> Option<LibraryEntry> {
    let game = game_ref(&t.app_name)?;
    let records = match (ubisoft_of(t), &t.third_party) {
        (Some(u), _) => vec![StoreRecord { game: u, relation: Relation::Linked, owned_by: Vec::new(), note: Some("Managed by Ubisoft Connect".into()) }],
        _ => Vec::new(),
    };
    Some(LibraryEntry {
        game,
        name: t.title.clone(),
        installed: installed.and_then(game_of),
        update_available: false,
        running: false,
        art: Art { epic: SpaceArt { tile: t.art.tall.clone(), hero: t.art.wide.clone(), logo: t.art.logo.clone(), icon: None }, ..Default::default() },
        owned_by: t.owners.clone(),
        // Ubisoft games sold on Epic install through Ubisoft; other launchers' games aren't tiles.
        installable: t.third_party.is_none(),
        steam: None,
        space_id: None,
        client: None,
        records,
    })
}

fn app_name(game: &GameRef) -> Result<&str> {
    if game.store != StoreId::Epic {
        bail!("{game} is not an Epic game");
    }
    Ok(game.id.as_str())
}

impl EpicProvider {
    async fn title(&self, game: &GameRef, refresh: bool) -> Result<(Title, Vec<quiver_api::epic::EpicAccount>)> {
        let app = app_name(game)?.to_owned();
        let lib = self.epic.library(refresh).await?;
        let accounts = self.epic.accounts()?;
        match lib.title(&app) {
            Some(t) => Ok((t.clone(), accounts)),
            None if accounts.is_empty() => bail!("{game}: no Epic account is signed in (quiver epic login)"),
            None => match lib.hidden(&app) {
                Some(h) => bail!("{game} isn't in the library: {}", h.reason),
                None => bail!("{game} is not in your Epic library"),
            },
        }
    }
}

impl StoreProvider for EpicProvider {
    fn id(&self) -> StoreId {
        StoreId::Epic
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    fn account(&self) -> AccountInfo {
        let accounts = match self.epic.accounts() {
            Ok(a) => a,
            Err(e) => return AccountInfo { state: AccountState::NeedsSignIn, name: None, note: Some(format!("{e:#}")) },
        };
        let Some(default) = accounts.iter().find(|a| a.default).or(accounts.first()) else {
            return AccountInfo { state: AccountState::NeedsSignIn, name: None, note: Some("sign in with `quiver epic login`".into()) };
        };
        let ended: Vec<&str> = accounts.iter().filter(|a| a.state == AccountState::NeedsSignIn).map(|a| a.display_name.as_str()).collect();
        let note = (!ended.is_empty()).then(|| format!("sign in again: {} (quiver epic login)", ended.join(", ")));
        let state = if accounts.iter().any(|a| a.state == AccountState::SignedIn) { AccountState::SignedIn } else { AccountState::NeedsSignIn };
        AccountInfo { state, name: Some(default.display_name.clone()), note }
    }

    fn library(&self, refresh: bool) -> BoxFuture<'_, Result<Vec<LibraryEntry>>> {
        Box::pin(async move {
            let lib = self.epic.library(refresh).await?;
            for e in &lib.errors {
                tracing::info!("Epic library: {e}");
            }
            let installed = self.epic.installed().unwrap_or_default();
            Ok(lib.titles.iter().filter_map(|t| entry(t, installed.iter().find(|i| i.app_name == t.app_name || t.apps.contains(&i.app_name)))).collect())
        })
    }

    fn detail(&self, game: GameRef, refresh: bool) -> BoxFuture<'_, Result<GameDetail>> {
        Box::pin(async move {
            let (t, accounts) = self.title(&game, refresh).await?;
            let owner = t.owners.first().and_then(|id| accounts.iter().find(|a| &a.account_id == id)).map(|a| a.display_name.clone());
            let dlc = t
                .dlc
                .iter()
                .filter_map(|d| {
                    let r = game_ref(d.app_name.as_deref().unwrap_or(&d.catalog_item_id))?;
                    Some(Dlc { game: r, name: Some(d.title.clone()).filter(|n| !n.is_empty()), installed: false, owned: Some(d.owned) })
                })
                .collect();
            let unavailable = vec!["release date".to_owned(), "play time".to_owned(), "achievements".to_owned(), "cloud saves".to_owned()];
            let third_party = t.third_party.is_some();
            let mut d = GameDetail {
                game,
                name: t.title.clone(),
                capabilities: capabilities(),
                kind: Some("game".into()),
                developer: t.developer.clone(),
                publisher: None,
                release_date: None,
                store_url: None,
                install: self.epic.installed_game(&t.app_name).map(|i| quiver_api::stores::ClientInstall {
                    dir: i.dir.clone(),
                    library: None,
                    size_bytes: i.size,
                    complete: true,
                    update_pending: false,
                    build_id: Some(i.version.clone()),
                    platforms: vec!["windows".into()],
                    ..Default::default()
                }),
                playtime: None,
                achievements: None,
                cloud: None,
                dlc,
                account: owner,
                unavailable,
                steam_hook: None,
                description: t.description.clone(),
                linked: ubisoft_of(&t),
                extras: self.epic.extras_of(&t, None),
            };
            // Play time, achievements and cloud saves (phase 3; `progress`). A game another
            // store's client manages keeps its progress there.
            if !third_party {
                self.epic.fill_detail(&mut d, refresh).await;
            }
            Ok(d)
        })
    }

    fn playtime(&self, game: GameRef) -> BoxFuture<'_, Result<Playtime>> {
        Box::pin(async move {
            app_name(&game)?;
            // Sessions Quiver recorded (none until Epic games launch through Quiver).
            quiver_core::settings::playtime(game)
        })
    }

    fn launch<'a>(&'a self, _source: &'a str, game: GameRef, _profile: Option<String>) -> BoxFuture<'a, Result<Launched>> {
        Box::pin(async move {
            let (t, _) = self.title(&game, false).await?;
            if let Some(u) = ubisoft_of(&t) {
                bail!("{} is a Ubisoft Connect game: once it is on your Ubisoft account, play it as {u}", t.title);
            }
            if let Some(tp) = &t.third_party {
                bail!("{} is managed by {}, which Quiver doesn't run yet", t.title, tp.client_name());
            }
            bail!("{} isn't installed: install it first (quiver install epic:{})", t.title, t.app_name)
        })
    }
}
