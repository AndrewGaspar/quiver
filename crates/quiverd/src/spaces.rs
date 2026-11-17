//! UbiServices' games API for the library (`quiver_core::spaces`): art for games whose
//! product configs have none (games bought on Steam), release dates, descriptions and
//! links for game info. Asked on the default account's session, a batch of space ids per
//! request, and kept on disk for a day (`$XDG_CACHE_HOME/quiver/spaces.json`).
//!
//! Fails soft: without an account, or when Ubisoft doesn't answer, callers get what the
//! cache has (possibly nothing) and fall through to their next source. After a failure it
//! waits a few minutes before asking again, so the library doesn't retry on every call.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use quiver_core::spaces::{self, SpaceCache, SpaceGame};

use crate::{Daemon, run_local};

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

struct State {
    cache: Option<SpaceCache>,
    failed_at: Option<Instant>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State { cache: None, failed_at: None }));
/// One fetch at a time (concurrent library calls ask for the same spaces).
static FETCHING: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);

fn cache_path() -> std::path::PathBuf {
    crate::info::cache_dir().join("spaces.json")
}

fn cached(ids: &[String]) -> HashMap<String, SpaceGame> {
    let mut st = STATE.lock().unwrap();
    let cache = st.cache.get_or_insert_with(|| SpaceCache::load(&cache_path()));
    ids.iter().filter_map(|id| Some((id.clone(), cache.get(id)?.clone()))).collect()
}

/// The games API's answers for `ids` (only the spaces it knows), from the cache when fresh.
/// `refresh` asks again for all of them.
pub(crate) async fn games(daemon: &Arc<Daemon>, ids: Vec<String>, refresh: bool) -> HashMap<String, SpaceGame> {
    let _one = FETCHING.lock().await;
    let stale = {
        let mut st = STATE.lock().unwrap();
        let backing_off = st.failed_at.is_some_and(|t| t.elapsed() < RETRY_AFTER);
        let cache = st.cache.get_or_insert_with(|| SpaceCache::load(&cache_path()));
        let now = chrono::Utc::now();
        if refresh {
            cache.stale(ids.iter().map(String::as_str), now + spaces::TTL + chrono::TimeDelta::seconds(1))
        } else if backing_off {
            Vec::new()
        } else {
            cache.stale(ids.iter().map(String::as_str), now)
        }
    };
    if !stale.is_empty() {
        match fetch(daemon, stale.clone()).await {
            Ok(found) => {
                tracing::info!(asked = stale.len(), found = found.len(), "games API");
                let mut st = STATE.lock().unwrap();
                st.failed_at = None;
                let cache = st.cache.get_or_insert_with(|| SpaceCache::load(&cache_path()));
                cache.record(&stale, found, chrono::Utc::now());
                if let Err(e) = cache.save() {
                    tracing::warn!("saving {}: {e:#}", cache_path().display());
                }
            }
            Err(e) => {
                tracing::info!(asked = stale.len(), "games API (art, release dates): {e:#}");
                STATE.lock().unwrap().failed_at = Some(Instant::now());
            }
        }
    }
    cached(&ids)
}

async fn fetch(daemon: &Arc<Daemon>, ids: Vec<String>) -> anyhow::Result<Vec<SpaceGame>> {
    let online = daemon.accounts.get(None)?;
    let work = run_local(move || async move {
        let session = online.session().await?;
        spaces::fetch(&online.auth, &session, &ids).await
    });
    tokio::time::timeout(FETCH_TIMEOUT, work).await.map_err(|_| anyhow::anyhow!("no answer in {} s", FETCH_TIMEOUT.as_secs()))?
}

/// One space's answer, if the cache has it (or it can be fetched).
pub(crate) async fn game(daemon: &Arc<Daemon>, space_id: &str, refresh: bool) -> Option<SpaceGame> {
    games(daemon, vec![space_id.to_owned()], refresh).await.remove(space_id)
}
