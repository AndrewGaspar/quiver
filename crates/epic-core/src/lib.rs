//! The Epic Games Store in Quiver (design/STORE-EPIC.md): accounts, the owned library and
//! catalog, and the store provider quiverd routes `epic:<app>` games to.
//!
//! Phase 1 as built (2026-11-17):
//!
//! - **Accounts** ([`Epic`]): sign-in with a pasted authorization code ([`auth`]), several
//!   accounts with a default ([`accounts`]), sessions in the keyring as `epic:<account id>`
//!   ([`secrets`]), refresh with Epic's rotating refresh tokens (single flight per account,
//!   new tokens stored before use), sign-out. A session Epic stops accepting marks the
//!   account "sign in again"; Quiver never signs in again on its own.
//! - **Library** ([`library`]): owned items and their catalog entries, art, DLC lists, and
//!   Ubisoft games sold on Epic (`ThirdPartyManaged*`, `GameID`), which the merged library
//!   folds into their Ubisoft entries.
//! - **Provider** ([`provider::EpicProvider`]): Epic games in the library as "not
//!   installed", their pages, and Play refused with the reason. Installs and launches are
//!   phase 2.
//!
//! Every Epic call goes through [`http::Transport`]; tests use [`http::Fake`] with recorded
//! JSON. Nothing here reads another launcher's files (legendary's `user.json`, Heroic's or
//! EGL's data): only Quiver's own sign-in creates an Epic session.

pub mod accounts;
pub mod api;
pub mod auth;
pub mod config;
pub mod http;
pub mod library;
pub mod provider;
pub mod secrets;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures::StreamExt;
use quiver_api::epic::{EpicAccount, LinkedAccount, LoginStart};
use quiver_core::secret::Secret;

use accounts::{Entry, Index};
use api::{Api, EpicError, Grant};
use auth::Tokens;
use config::ClientConfig;
use library::{Cache, CachedItem, CachedLibrary, Title};
use secrets::{SecretStore, entry_name};

/// How long a library read is reused before Epic is asked again.
pub const LIBRARY_TTL: Duration = Duration::from_secs(15 * 60);
/// Catalog requests in flight at once (one per namespace; EGL makes them too).
const CATALOG_CONCURRENCY: usize = 4;

/// The Epic library across signed-in accounts.
#[derive(Debug, Clone, Default)]
pub struct Library {
    pub titles: Vec<Title>,
    /// What couldn't be read (an account signed out, Epic unreachable): the titles then
    /// come from the last good read, if any.
    pub errors: Vec<String>,
}

impl Library {
    pub fn title(&self, app_name: &str) -> Option<&Title> {
        self.titles.iter().find(|t| t.app_name == app_name)
    }
}

/// Epic's state in quiverd: one per daemon.
pub struct Epic {
    dir: PathBuf,
    api: Api,
    secrets: Arc<dyn SecretStore>,
    cache: Cache,
    /// Sessions read this run (the keyring is the record).
    tokens: tokio::sync::Mutex<HashMap<String, Tokens>>,
    /// One refresh at a time per account.
    refreshing: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// One library read at a time, and the last one.
    library: tokio::sync::Mutex<Option<(Instant, Arc<Library>)>>,
}

impl Epic {
    /// Epic under `<data>/epic` (`data_dir` is Quiver's data directory, which also holds
    /// the client override `epic-client.json`).
    pub fn new(data_dir: &Path, transport: Arc<dyn http::Transport>, secrets: Arc<dyn SecretStore>) -> Self {
        let (config, _) = ClientConfig::load(data_dir);
        Self::with_config(data_dir, config, transport, secrets)
    }

    pub fn with_config(data_dir: &Path, config: ClientConfig, transport: Arc<dyn http::Transport>, secrets: Arc<dyn SecretStore>) -> Self {
        let dir = data_dir.join("epic");
        Epic {
            cache: Cache::new(&dir),
            dir,
            api: Api::new(transport, config),
            secrets,
            tokens: Default::default(),
            refreshing: Default::default(),
            library: Default::default(),
        }
    }

    /// The real thing: HTTPS with the configured user agent, sessions in the keyring.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let (config, _) = ClientConfig::load(data_dir);
        let transport = Arc::new(http::Reqwest::new(&config.user_agent)?);
        Ok(Self::with_config(data_dir, config, transport, Arc::new(secrets::Keyring)))
    }

    pub fn config(&self) -> &ClientConfig {
        &self.api.config
    }

    // ---- accounts

    fn index(&self) -> Result<Index> {
        Index::load(&self.dir)
    }

    pub fn accounts(&self) -> Result<Vec<EpicAccount>> {
        Ok(self.index()?.list())
    }

    pub fn login_start(&self) -> LoginStart {
        LoginStart { url: auth::login_url(self.config()), instructions: auth::INSTRUCTIONS.into() }
    }

    /// Exchange a pasted code for a session and add the account (signing in again
    /// replaces its session). The session is in the keyring before the account is listed.
    pub async fn login(&self, pasted: &str) -> Result<EpicAccount> {
        let code = auth::parse_code(pasted)?;
        let t = self.api.token(Grant::AuthorizationCode(&code)).await.map_err(explain_login)?;
        let tokens = Tokens::from_response(&t, Utc::now())?;
        let access = tokens.access_token.expose().clone();
        let id = tokens.account_id.clone();
        // The display name and the linked Ubisoft account; neither is worth failing for.
        let name = match self.api.account(&access, &id).await {
            Ok(a) if !a.display_name.is_empty() => a.display_name,
            Ok(_) => tokens.display_name.clone(),
            Err(e) => {
                tracing::info!("Epic account details: {e:#}");
                tokens.display_name.clone()
            }
        };
        let ubisoft = match self.api.external_auths(&access, &id).await {
            Ok(list) => list
                .into_iter()
                .find(|x| x.kind.eq_ignore_ascii_case("ubisoft") && !x.external_auth_id.is_empty())
                .map(|x| LinkedAccount { id: x.external_auth_id, name: x.external_display_name.filter(|n| !n.is_empty()) }),
            Err(e) => {
                tracing::info!("Epic linked accounts: {e:#}");
                None
            }
        };
        let tokens = Tokens { display_name: if name.is_empty() { id.clone() } else { name.clone() }, ..tokens };
        self.store_tokens(&tokens).await?;
        let mut index = self.index()?;
        index.upsert(Entry { account_id: id.clone(), display_name: tokens.display_name.clone(), ubisoft, needs_sign_in: false });
        index.save(&self.dir)?;
        self.tokens.lock().await.insert(id.clone(), tokens);
        self.forget_library().await;
        index.list().into_iter().find(|a| a.account_id == id).context("account just added")
    }

    /// Sign an account out: end its session at Epic (best effort), delete it from the
    /// keyring, take it off the list.
    pub async fn logout(&self, query: Option<&str>) -> Result<EpicAccount> {
        let mut index = self.index()?;
        let entry = index.resolve(query)?.clone();
        let id = entry.account_id.clone();
        let account = index.list().into_iter().find(|a| a.account_id == id).context("account")?;
        if !entry.needs_sign_in {
            match self.load_tokens(&id).await {
                Ok(Some(t)) if !t.needs_refresh(Utc::now()) => {
                    if let Err(e) = self.api.kill_session(t.access_token.expose()).await {
                        tracing::info!("ending the Epic session: {e:#}");
                    }
                }
                _ => {} // expired anyway: nothing to end
            }
        }
        let secrets = self.secrets.clone();
        let name = entry_name(&id);
        tokio::task::spawn_blocking(move || secrets.delete(&name)).await??;
        self.tokens.lock().await.remove(&id);
        index.remove(&id);
        index.save(&self.dir)?;
        self.cache.forget_library(&id);
        self.forget_library().await;
        Ok(account)
    }

    pub fn set_default(&self, query: &str) -> Result<EpicAccount> {
        let mut index = self.index()?;
        let id = index.resolve(Some(query))?.account_id.clone();
        index.default = Some(id.clone());
        index.save(&self.dir)?;
        index.list().into_iter().find(|a| a.account_id == id).context("account")
    }

    async fn store_tokens(&self, t: &Tokens) -> Result<()> {
        let json = serde_json::to_string(t)?;
        let secrets = self.secrets.clone();
        let name = entry_name(&t.account_id);
        tokio::task::spawn_blocking(move || secrets.set(&name, &json)).await?
    }

    async fn load_tokens(&self, account_id: &str) -> Result<Option<Tokens>> {
        if let Some(t) = self.tokens.lock().await.get(account_id) {
            return Ok(Some(t.clone()));
        }
        let secrets = self.secrets.clone();
        let name = entry_name(account_id);
        let Some(json) = tokio::task::spawn_blocking(move || secrets.get(&name)).await?? else { return Ok(None) };
        let t: Tokens = serde_json::from_str(&json).context("corrupt stored Epic session")?;
        self.tokens.lock().await.insert(account_id.to_owned(), t.clone());
        Ok(Some(t))
    }

    /// A usable access token for an account, refreshing it first when it is close to
    /// expiring. Epic rotates the refresh token, so the new pair goes to the keyring
    /// before the new access token is used. A refused refresh marks the account "sign in
    /// again" and is not retried.
    pub async fn access_token(&self, account_id: &str) -> Result<Secret<String>> {
        let lock = self.refreshing.lock().unwrap().entry(account_id.to_owned()).or_default().clone();
        let _flight = lock.lock().await;
        let entry = self.index()?.accounts.into_iter().find(|a| a.account_id == account_id).with_context(|| format!("no Epic account {account_id}"))?;
        if entry.needs_sign_in {
            bail!("{}'s Epic session ended: sign in again (quiver epic login)", entry.display_name);
        }
        let Some(t) = self.load_tokens(account_id).await? else {
            self.mark_signed_out(account_id).await?;
            bail!("{}'s Epic session is missing from the keyring: sign in again (quiver epic login)", entry.display_name);
        };
        let now = Utc::now();
        if !t.needs_refresh(now) {
            return Ok(t.access_token.clone());
        }
        if t.refresh_expired(now) {
            self.mark_signed_out(account_id).await?;
            bail!("{}'s Epic session ended: sign in again (quiver epic login)", entry.display_name);
        }
        match self.api.token(Grant::RefreshToken(t.refresh_token.expose())).await {
            Ok(r) => {
                let mut fresh = Tokens::from_response(&r, Utc::now())?;
                if fresh.account_id != t.account_id {
                    bail!("Epic refreshed the session for a different account");
                }
                fresh.display_name = t.display_name.clone();
                // Stored first: the old refresh token is dead now.
                self.store_tokens(&fresh).await?;
                let access = fresh.access_token.clone();
                self.tokens.lock().await.insert(account_id.to_owned(), fresh);
                Ok(access)
            }
            Err(e) if e.downcast_ref::<EpicError>().is_some_and(|x| x.is_invalid_grant() || x.is_corrective_action()) => {
                self.mark_signed_out(account_id).await?;
                Err(e.context(format!("{}'s Epic session ended: sign in again (quiver epic login)", entry.display_name)))
            }
            Err(e) => Err(e.context("refresh the Epic session")),
        }
    }

    async fn mark_signed_out(&self, account_id: &str) -> Result<()> {
        let mut index = self.index()?;
        if let Some(a) = index.get_mut(account_id) {
            a.needs_sign_in = true;
        }
        index.save(&self.dir)?;
        self.tokens.lock().await.remove(account_id);
        let secrets = self.secrets.clone();
        let name = entry_name(account_id);
        tokio::task::spawn_blocking(move || secrets.delete(&name)).await??;
        tracing::warn!(account_id, "the Epic session ended; the account needs to sign in again");
        Ok(())
    }

    // ---- library

    async fn forget_library(&self) {
        *self.library.lock().await = None;
    }

    /// The library across accounts, from the last read when it is recent (`refresh`: ask
    /// Epic again). With no Epic account, empty, and nothing is asked.
    pub async fn library(&self, refresh: bool) -> Result<Arc<Library>> {
        let mut slot = self.library.lock().await;
        if let Some((at, lib)) = slot.as_ref()
            && !refresh
            && at.elapsed() < LIBRARY_TTL
        {
            return Ok(lib.clone());
        }
        let lib = Arc::new(self.read_library(refresh).await?);
        *slot = Some((Instant::now(), lib.clone()));
        Ok(lib)
    }

    /// The last library read, without asking Epic (`None`: nothing read yet this run).
    pub async fn cached_library(&self) -> Option<Arc<Library>> {
        self.library.lock().await.as_ref().map(|(_, l)| l.clone())
    }

    async fn read_library(&self, refresh: bool) -> Result<Library> {
        let index = self.index()?;
        let mut per_account = Vec::new();
        let mut errors = Vec::new();
        let mut tokens = Vec::new();
        for a in &index.accounts {
            let fetched = match self.access_token(&a.account_id).await {
                Ok(t) => {
                    let r = self.api.library_items(t.expose()).await;
                    tokens.push(t);
                    r
                }
                Err(e) => Err(e),
            };
            match fetched {
                Ok(records) => {
                    if let Err(e) = self.cache.save_library(&a.account_id, &CachedLibrary { fetched_at: Utc::now(), records: records.clone() }) {
                        tracing::info!("Epic library cache: {e:#}");
                    }
                    per_account.push((a.account_id.clone(), records));
                }
                Err(e) => {
                    let cached = self.cache.library(&a.account_id);
                    let when = cached.as_ref().map(|c| format!("; showing what Epic said on {}", c.fetched_at.format("%Y-%m-%d"))).unwrap_or_default();
                    errors.push(format!("{}: {e:#}{when}", a.display_name));
                    if let Some(c) = cached {
                        per_account.push((a.account_id.clone(), c.records));
                    }
                }
            }
        }
        let catalog = self.catalog_for(&per_account, tokens.first(), refresh, &mut errors).await;
        Ok(Library { titles: library::build(&per_account, &catalog), errors })
    }

    /// Catalog items for every record, from the cache when fresh, else from Epic (one call
    /// per namespace). Items Epic doesn't return keep their cached copy.
    async fn catalog_for(
        &self,
        per_account: &[(String, Vec<api::LibraryRecord>)],
        token: Option<&Secret<String>>,
        refresh: bool,
        errors: &mut Vec<String>,
    ) -> HashMap<String, api::CatalogItem> {
        let mut cached = self.cache.catalog();
        let now = Utc::now();
        let mut wanted: HashMap<String, Vec<String>> = HashMap::new();
        for (_, records) in per_account {
            for r in records.iter().filter(|r| r.namespace != "ue" && !r.catalog_item_id.is_empty()) {
                let key = library::catalog_key(&r.namespace, &r.catalog_item_id);
                let stale = cached.get(&key).is_none_or(|c| refresh || now - c.fetched_at > library::CATALOG_TTL);
                if stale {
                    let ids = wanted.entry(r.namespace.clone()).or_default();
                    if !ids.contains(&r.catalog_item_id) {
                        ids.push(r.catalog_item_id.clone());
                    }
                }
            }
        }
        if let (Some(token), false) = (token, wanted.is_empty()) {
            let jobs: Vec<(String, Vec<String>)> =
                wanted.into_iter().flat_map(|(ns, ids)| ids.chunks(api::CATALOG_BATCH).map(|c| (ns.clone(), c.to_vec())).collect::<Vec<_>>()).collect();
            let results: Vec<_> = futures::stream::iter(jobs)
                .map(|(ns, ids)| async move { (ns.clone(), self.api.catalog_items(token.expose(), &ns, &ids).await) })
                .buffer_unordered(CATALOG_CONCURRENCY)
                .collect()
                .await;
            let mut failed = 0;
            for (ns, r) in results {
                match r {
                    Ok(items) => {
                        for (_, item) in items {
                            let key = library::catalog_key(if item.namespace.is_empty() { &ns } else { &item.namespace }, &item.id);
                            cached.insert(key, CachedItem { fetched_at: now, item });
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::info!(namespace = ns, "Epic catalog: {e:#}");
                    }
                }
            }
            if failed > 0 {
                errors.push(format!("Epic's catalog didn't answer for {failed} game(s); their names and art may be missing"));
            }
            if let Err(e) = self.cache.save_catalog(&cached) {
                tracing::info!("Epic catalog cache: {e:#}");
            }
        }
        cached.into_iter().map(|(k, c)| (k, c.item)).collect()
    }
}

/// Make a failed code exchange say what to do.
fn explain_login(e: anyhow::Error) -> anyhow::Error {
    match e.downcast_ref::<EpicError>() {
        Some(x) if x.is_corrective_action() => anyhow::anyhow!("{x}"),
        Some(x) if x.is_invalid_grant() => anyhow::anyhow!("Epic didn't accept that code (it works once, for a few minutes): open the sign-in link again and paste the new code"),
        _ => e.context("sign in to Epic"),
    }
}

#[cfg(test)]
mod tests;
