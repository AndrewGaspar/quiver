//! Store providers (design/STORES.md §3): the seam each store sits behind.
//!
//! quiverd keeps one queue, one launcher and one library, and routes each request that
//! names a game by its `game.store` to that store's provider. A provider says what it
//! implements ([`Capabilities`]) so front ends show what isn't wired instead of inventing
//! it. Ubisoft's provider wraps the code quiverd had before providers (its library,
//! launcher and bridge); Steam's (`steamlocal::SteamLocal`) reads the Steam client's own
//! files and plays through the client.
//!
//! This is the first cut (R2-lite): library with art, a game's page, play time and launch.
//! Accounts, installers, cloud saves, progress and entitlements (§3.2–§3.10) get their
//! traits when the first store that needs them arrives.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
pub use quiver_api::stores::{AccountState, Capabilities, GameDetail, Support};
use quiver_api::{GameRef, LibraryEntry, Playtime, StoreId};

/// A boxed, sendable future, so providers can be used as `dyn StoreProvider`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a launch went (`StoreProvider::launch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launched {
    /// Quiver runs the game: the provider queued this Play job.
    Job(u64),
    /// The store's client was asked to start it (`steam://rungameid/620`): what was run.
    /// The client plays it; quiverd records a finished Play job and an activity line.
    Delegated { url: String, argv: Vec<String> },
}

/// Who a store's games are read for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccountInfo {
    pub state: AccountState,
    /// The account's name, when known.
    pub name: Option<String>,
    /// Why there is no account or no games ("Steam not found").
    pub note: Option<String>,
}

pub trait StoreProvider: Send + Sync {
    fn id(&self) -> StoreId;

    fn capabilities(&self) -> Capabilities;

    fn account(&self) -> AccountInfo;

    /// The store's games for the library, with art (`LibraryEntry::art`, `client.art`) and
    /// install state. `refresh` re-reads what the provider caches.
    fn library(&self, refresh: bool) -> BoxFuture<'_, Result<Vec<LibraryEntry>>>;

    /// A game's page: facts, install, play time, achievements, cloud state.
    fn detail(&self, game: GameRef, refresh: bool) -> BoxFuture<'_, Result<GameDetail>>;

    /// Play time: Quiver's sessions, or what the store recorded (no sessions then).
    fn playtime(&self, game: GameRef) -> BoxFuture<'_, Result<Playtime>>;

    /// Start the game. `profile` is a mod profile (stores whose launches Quiver runs only).
    fn launch<'a>(&'a self, source: &'a str, game: GameRef, profile: Option<String>) -> BoxFuture<'a, Result<Launched>>;
}

/// Find a game's provider.
pub fn provider_for<'a>(providers: &'a [std::sync::Arc<dyn StoreProvider>], game: &GameRef) -> Result<&'a dyn StoreProvider> {
    providers
        .iter()
        .find(|p| p.id() == game.store)
        .map(|p| p.as_ref())
        .ok_or_else(|| anyhow::anyhow!("{game}: Quiver has no {} store yet", game.store.label()))
}

/// The library across stores. Records merge only on explicit links (STORES.md §2.4, steer
/// X2): a Ubisoft game bought on Steam is already one Ubisoft entry with its Steam install
/// in `steam`, so the Steam provider's entry for that app is dropped. Nothing merges on
/// names. Installed games first, then by name, as before.
pub fn merge_libraries(mut parts: Vec<Vec<LibraryEntry>>) -> Vec<LibraryEntry> {
    let linked: std::collections::HashSet<GameRef> =
        parts.iter().flatten().filter_map(|e| e.steam.as_ref()).map(|s| GameRef::steam(s.app_id)).collect();
    let mut out: Vec<LibraryEntry> = Vec::new();
    for part in parts.drain(..) {
        out.extend(part.into_iter().filter(|e| e.game.is_ubisoft() || !linked.contains(&e.game)));
    }
    out.sort_by(|a, b| b.on_disk().cmp(&a.on_disk()).then_with(|| a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(game: GameRef, name: &str, steam_app: Option<u32>, on_disk: bool) -> LibraryEntry {
        let mut e: LibraryEntry =
            serde_json::from_value(serde_json::json!({"game": game, "name": name, "installed": null, "update_available": false, "running": false})).unwrap();
        if let Some(app_id) = steam_app {
            e.steam = Some(serde_json::from_value(serde_json::json!({"app_id": app_id, "name": name, "install_dir": "/s", "state": "off"})).unwrap());
        }
        if on_disk && steam_app.is_none() && !game.is_ubisoft() {
            e.client = Some(Default::default());
        }
        e
    }

    #[test]
    fn linked_copies_stay_merged_and_names_never_merge() {
        let ubisoft = vec![entry(GameRef::ubisoft(66088), "Black Flag Resynced", Some(3751950), true), entry(GameRef::ubisoft(3539), "Origins", None, false)];
        let steam = vec![
            entry(GameRef::steam(3751950), "Black Flag Resynced", None, true),
            entry(GameRef::steam(582160), "Origins", None, true),
            entry(GameRef::steam(620), "Portal 2", None, true),
        ];
        let lib = merge_libraries(vec![ubisoft, steam]);
        let refs: Vec<String> = lib.iter().map(|e| e.game.to_string()).collect();
        // The linked Steam copy is folded into its Ubisoft entry; the same name in another
        // store without a link stays its own entry. Installed first, then by name.
        assert_eq!(refs, ["ubisoft:66088", "steam:582160", "steam:620", "ubisoft:3539"]);
    }
}
